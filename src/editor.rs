/*!
 * An in-terminal level editor.
 *
 * The editor owns the level as ASCII (`meta` + `rows`) because the game's
 * [`LevelDefinition`] collapses `G` and `P` into one solid bit. A snapshot is
 * built from the grid for the renderer and for [`validator`], which is the same
 * code `--validate` runs.
 *
 * The level is drawn by the game's own renderer into the usual 360x270 frame;
 * the grid, cursor, spawn/goal outlines, reached tint, invalid cells and
 * unreachable-coin marks are blended into that frame. Text (the HUD and the
 * modal prompts) is printed by the terminal over the image.
 *
 * Validation runs on a worker thread after a short debounce, so the editor
 * stays responsive while the search runs. A result is dropped when the level
 * has changed since the job started.
 */

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, queue};
use image::RgbaImage;

use crate::audio::MUSIC_NAMES;
use crate::camera::Camera;
use crate::color::Palette;
use crate::level::{LevelDefinition, TILE_SIZE, parse_palette};
use crate::render::{self, BACKGROUNDS};
use crate::terminal::{Terminal, kitty_graphics};
use crate::validator::{self, Report};

const VIEW_WIDTH: u32 = 360;
const VIEW_HEIGHT: u32 = 270;

const MIN_WIDTH: usize = 16;
const MAX_WIDTH: usize = 256;
const MIN_HEIGHT: usize = 8;
const MAX_HEIGHT: usize = 64;

const HUD_LINES: u16 = 3;
const CHECK_DELAY: Duration = Duration::from_millis(200);
const MESSAGE_TIME: Duration = Duration::from_secs(3);

const DEFAULT_MUSIC: &str = "echoes-of-solace";
const DEFAULT_BACKGROUND: &str = "snow";
const DEFAULT_PALETTE_TEXT: &str = "#071A1D, #174B45, #2F8F5B, #A8D96F";
const DEFAULT_PALETTE: Palette = [
    [0x07, 0x1A, 0x1D],
    [0x17, 0x4B, 0x45],
    [0x2F, 0x8F, 0x5B],
    [0xA8, 0xD9, 0x6F],
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Brush {
    Ground,
    Platform,
    Coin,
    Spawn,
    Goal,
    Erase,
}

impl Brush {
    const ALL: [Self; 6] = [
        Self::Ground,
        Self::Platform,
        Self::Coin,
        Self::Spawn,
        Self::Goal,
        Self::Erase,
    ];

    fn from_index(index: usize) -> Self {
        Self::ALL[index % Self::ALL.len()]
    }

    fn from_char(glyph: char) -> Option<Self> {
        Some(match glyph {
            'G' | 'g' => Self::Ground,
            'P' | 'p' => Self::Platform,
            'C' | 'c' => Self::Coin,
            'S' | 's' => Self::Spawn,
            'F' | 'f' => Self::Goal,
            '.' => Self::Erase,
            _ => return None,
        })
    }

    fn glyph(self) -> char {
        match self {
            Self::Ground => 'G',
            Self::Platform => 'P',
            Self::Coin => 'C',
            Self::Spawn => 'S',
            Self::Goal => 'F',
            Self::Erase => '.',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ground => "ground",
            Self::Platform => "platform",
            Self::Coin => "coin",
            Self::Spawn => "spawn",
            Self::Goal => "goal",
            Self::Erase => "erase",
        }
    }
}

#[derive(Debug, Clone)]
struct LevelDoc {
    meta: Vec<(String, String)>,
    rows: Vec<Vec<char>>,
}

impl LevelDoc {
    fn width(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }

    fn height(&self) -> usize {
        self.rows.len()
    }

    fn meta_value(&self, key: &str) -> Option<&str> {
        self.meta
            .iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, value)| value.as_str())
    }

    fn set_meta(&mut self, key: &str, value: &str) {
        if let Some((_, current)) =
            self.meta.iter_mut().find(|(name, _)| name.as_str() == key)
        {
            *current = value.to_string();
        } else {
            self.meta.push((key.to_string(), value.to_string()));
        }
    }

    fn positions(&self, glyph: char) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (y, row) in self.rows.iter().enumerate() {
            for (x, &cell) in row.iter().enumerate() {
                if cell == glyph {
                    out.push((x, y));
                }
            }
        }
        out
    }

    fn spawns(&self) -> Vec<(usize, usize)> {
        self.positions('S')
    }

    fn goals(&self) -> Vec<(usize, usize)> {
        self.positions('F')
    }

    fn coins(&self) -> Vec<(usize, usize)> {
        self.positions('C')
    }

    fn spawn(&self) -> Option<(usize, usize)> {
        self.spawns().into_iter().next()
    }

    fn goal(&self) -> Option<(usize, usize)> {
        self.goals().into_iter().next()
    }

    /// Build the snapshot the renderer and the validator understand.
    ///
    /// A missing spawn or goal is placed far outside the level: the renderer
    /// then draws nothing where a marker would be, and the structural report
    /// says what is missing. The validator is never run in that state.
    fn to_definition(&self) -> LevelDefinition {
        let palette = self
            .meta_value("palette")
            .and_then(|value| parse_palette(value).ok())
            .unwrap_or(DEFAULT_PALETTE);
        let background = self
            .meta_value("background")
            .filter(|name| BACKGROUNDS.contains(name))
            .unwrap_or(DEFAULT_BACKGROUND);
        let width = self.width();
        let height = self.height();
        let missing = (width + 1000, height + 1000);
        let mut blocks = vec![vec![false; width]; height];
        let mut coins = Vec::new();
        for (y, row) in self.rows.iter().enumerate() {
            for (x, &glyph) in row.iter().enumerate() {
                match glyph {
                    'G' | 'P' => blocks[y][x] = true,
                    'C' => coins.push((x, y)),
                    _ => {}
                }
            }
        }
        LevelDefinition {
            name: self.meta_value("name").unwrap_or("New Level").to_string(),
            music: self
                .meta_value("music")
                .unwrap_or(DEFAULT_MUSIC)
                .to_string(),
            background: background.to_string(),
            palette,
            width,
            height,
            spawn: self.spawn().unwrap_or(missing),
            goal: self.goal().unwrap_or(missing),
            blocks,
            coins,
        }
    }

    fn serialize(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.meta {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out.push('\n');
        for row in &self.rows {
            for &cell in row {
                out.push(cell);
            }
            out.push('\n');
        }
        out
    }
}

/// Parse the game's level text, padding ragged rows so the grid is rectangular.
fn parse_doc(text: &str) -> Result<(LevelDoc, Vec<String>)> {
    let mut lines = text.lines();
    let mut meta = Vec::new();
    let mut separated = false;

    for line in lines.by_ref() {
        if line.trim().is_empty() {
            separated = true;
            break;
        }
        let (key, value) = line
            .split_once(':')
            .with_context(|| format!("expected 'key: value', got {line:?}"))?;
        meta.push((key.trim().to_string(), value.trim().to_string()));
    }

    if !separated {
        bail!("the metadata needs a blank line before the grid");
    }

    let mut rows: Vec<Vec<char>> =
        lines.map(|line| line.chars().collect()).collect();
    if rows.is_empty() {
        bail!("the level has no grid");
    }

    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        bail!("the level's grid is empty");
    }

    let mut warnings = Vec::new();
    for (index, row) in rows.iter_mut().enumerate() {
        if row.len() != width {
            warnings.push(format!(
                "row {} is {} wide, not {} (padded with air)",
                index + 1,
                row.len(),
                width
            ));
            row.resize(width, '.');
        }
    }

    Ok((LevelDoc { meta, rows }, warnings))
}

fn blank_doc(name: &str, width: usize, height: usize) -> LevelDoc {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
    let mut rows = vec![vec!['.'; width]; height];
    rows[height - 1].fill('G');
    rows[height - 2][2] = 'S';
    rows[height - 2][width - 3] = 'F';
    LevelDoc {
        meta: vec![
            ("name".to_string(), name.to_string()),
            ("music".to_string(), DEFAULT_MUSIC.to_string()),
            ("background".to_string(), DEFAULT_BACKGROUND.to_string()),
            ("palette".to_string(), DEFAULT_PALETTE_TEXT.to_string()),
        ],
        rows,
    }
}

/// Paint one cell. Returns `false` when the grid did not change.
fn paint_cell(doc: &mut LevelDoc, x: usize, y: usize, glyph: char) -> bool {
    if doc.rows[y][x] == glyph {
        return false;
    }
    // The game allows one spawn and one goal: painting either moves it.
    if glyph == 'S' || glyph == 'F' {
        for row in &mut doc.rows {
            for cell in row.iter_mut() {
                if *cell == glyph {
                    *cell = '.';
                }
            }
        }
    }
    doc.rows[y][x] = glyph;
    true
}

fn resize_doc(
    doc: &LevelDoc,
    width: usize,
    height: usize,
) -> std::result::Result<LevelDoc, String> {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);

    let mut cut = Vec::new();
    if let Some((x, y)) = doc.spawn()
        && (x >= width || y >= height)
    {
        cut.push("the spawn".to_string());
    }
    if let Some((x, y)) = doc.goal()
        && (x >= width || y >= height)
    {
        cut.push("the goal".to_string());
    }
    let lost = doc
        .coins()
        .into_iter()
        .filter(|&(x, y)| x >= width || y >= height)
        .count();
    if lost > 0 {
        cut.push(format!("{lost} coin(s)"));
    }
    if !cut.is_empty() {
        return Err(format!(
            "{}x{} would cut off {}",
            width,
            height,
            cut.join(", ")
        ));
    }

    let mut rows = Vec::with_capacity(height);
    for y in 0..height {
        let mut row = if y < doc.height() {
            doc.rows[y].iter().take(width).copied().collect()
        } else {
            Vec::new()
        };
        row.resize(width, '.');
        rows.push(row);
    }

    Ok(LevelDoc { meta: doc.meta.clone(), rows })
}

#[derive(Default)]
struct Structural {
    errors: Vec<String>,
    warnings: Vec<String>,
    marks: Vec<(usize, usize)>,
}

fn structural_report(doc: &LevelDoc, warnings: Vec<String>) -> Structural {
    let mut report = Structural { warnings, ..Structural::default() };

    for key in ["name", "music", "background", "palette"] {
        if doc.meta_value(key).is_none() {
            report.errors.push(format!("metadata '{key}' is missing"));
        }
    }
    if let Some(palette) = doc.meta_value("palette")
        && let Err(error) = parse_palette(palette)
    {
        report.errors.push(format!("palette: {error:#}"));
    }
    if let Some(music) = doc.meta_value("music")
        && !MUSIC_NAMES.contains(&music)
    {
        report.errors.push(format!("music '{music}' is not a game track"));
    }
    if let Some(background) = doc.meta_value("background")
        && !BACKGROUNDS.contains(&background)
    {
        report.errors.push(format!(
            "background '{background}' is not a game background"
        ));
    }

    let spawns = doc.spawns();
    match spawns.len() {
        0 => report
            .errors
            .push("no spawn: paint one with the spawn brush".to_string()),
        1 => {}
        count => {
            report
                .errors
                .push(format!("{count} spawns: the game allows exactly one"));
            report.marks.extend(spawns.iter().copied());
        }
    }

    let goals = doc.goals();
    match goals.len() {
        0 => report
            .errors
            .push("no goal: paint one with the goal brush".to_string()),
        1 => {}
        count => {
            report
                .errors
                .push(format!("{count} goals: the game allows exactly one"));
            report.marks.extend(goals.iter().copied());
        }
    }

    let mut unknown = 0;
    for (y, row) in doc.rows.iter().enumerate() {
        for (x, &glyph) in row.iter().enumerate() {
            if matches!(glyph, '.' | ' ' | 'G' | 'P' | 'C' | 'S' | 'F') {
                continue;
            }
            unknown += 1;
            if unknown <= 8 {
                report
                    .errors
                    .push(format!("unknown tile '{glyph}' at {x},{y}"));
                report.marks.push((x, y));
            }
        }
    }

    report
}

#[derive(Default)]
struct Check {
    result: Option<Receiver<(u64, Report)>>,
    running: bool,
    report: Option<Report>,
}

struct SizePrompt {
    new: bool,
    width: String,
    height: String,
    field: Field,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Width,
    Height,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Self::Width => Self::Height,
            Self::Height => Self::Width,
        }
    }
}

impl SizePrompt {
    fn field_mut(&mut self) -> &mut String {
        match self.field {
            Field::Width => &mut self.width,
            Field::Height => &mut self.height,
        }
    }

    fn line(&self) -> String {
        let width = if self.field == Field::Width {
            format!("[{}]", self.width)
        } else {
            self.width.clone()
        };
        let height = if self.field == Field::Height {
            format!("[{}]", self.height)
        } else {
            self.height.clone()
        };
        format!(
            " {} > width {} x height {}   (Tab, Enter, Esc)",
            if self.new { "new level" } else { "resize" },
            width,
            height
        )
    }
}

enum Overlay {
    None,
    Music(usize),
    Background(usize),
    Size(SizePrompt),
    Command(String),
    Help,
    ConfirmQuit,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Brush(char),
    Music(String),
    Background(String),
    New(usize, usize),
    Resize(usize, usize),
    Name(String),
    Save,
    Reload,
    Check,
    Grid(bool),
    Goto(usize, usize),
    Help,
    Quit,
}

fn parse_size(args: &[&str]) -> Option<(usize, usize)> {
    match args {
        [one] => {
            let (width, height) = one.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        }
        [width, height, ..] => {
            Some((width.parse().ok()?, height.parse().ok()?))
        }
        _ => None,
    }
}

fn parse_command(input: &str) -> std::result::Result<Command, String> {
    let mut words = input.split_whitespace();
    let name = words.next().ok_or_else(|| "empty command".to_string())?;
    let args: Vec<&str> = words.collect();

    match name {
        "brush" => {
            let glyph = args
                .first()
                .and_then(|arg| arg.chars().next())
                .ok_or_else(|| "brush needs a tile character".to_string())?;
            Ok(Command::Brush(glyph))
        }
        "music" => args
            .first()
            .map(|arg| Command::Music((*arg).to_string()))
            .ok_or_else(|| "music needs a name".to_string()),
        "background" => args
            .first()
            .map(|arg| Command::Background((*arg).to_string()))
            .ok_or_else(|| "background needs a name".to_string()),
        "new" | "resize" => {
            let (width, height) = parse_size(&args).ok_or_else(|| {
                "expected <width> <height> or <width>x<height>".to_string()
            })?;
            Ok(if name == "new" {
                Command::New(width, height)
            } else {
                Command::Resize(width, height)
            })
        }
        "name" => {
            let value = args.join(" ");
            if value.is_empty() {
                Err("name needs a value".to_string())
            } else {
                Ok(Command::Name(value))
            }
        }
        "save" => Ok(Command::Save),
        "reload" => Ok(Command::Reload),
        "check" => Ok(Command::Check),
        "grid" => match args.first().copied() {
            Some("on") => Ok(Command::Grid(true)),
            Some("off") => Ok(Command::Grid(false)),
            _ => Err("grid needs on or off".to_string()),
        },
        "goto" => {
            let x = args.first().and_then(|arg| arg.parse().ok());
            let y = args.get(1).and_then(|arg| arg.parse().ok());
            match (x, y) {
                (Some(x), Some(y)) => Ok(Command::Goto(x, y)),
                _ => Err("goto needs <x> <y>".to_string()),
            }
        }
        "help" => Ok(Command::Help),
        "quit" | "q" => Ok(Command::Quit),
        _ => Err(format!("unknown command '{name}'")),
    }
}

struct Editor {
    path: PathBuf,
    doc: LevelDoc,
    warnings: Vec<String>,
    structural: Structural,
    cursor: (usize, usize),
    brush: Brush,
    dirty: bool,
    grid: bool,
    overlay: Overlay,
    message: Option<(String, Instant)>,
    check: Check,
    generation: u64,
    last_edit: Instant,
    needs_check: bool,
    redraw: bool,
    quit: bool,
}

impl Editor {
    fn open(path: &Path) -> Result<Self> {
        let (doc, warnings, is_new) = if path.exists() {
            let text = fs::read_to_string(path).with_context(|| {
                format!("could not read {}", path.display())
            })?;
            let (doc, warnings) = parse_doc(&text).with_context(|| {
                format!("could not parse {}", path.display())
            })?;
            (doc, warnings, false)
        } else {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("New Level");
            (blank_doc(name, 40, 14), Vec::new(), true)
        };

        let structural = structural_report(&doc, warnings.clone());
        let cursor = doc.spawn().unwrap_or((0, 0));
        let mut editor = Self {
            path: path.to_path_buf(),
            doc,
            warnings,
            structural,
            cursor,
            brush: Brush::Ground,
            dirty: is_new,
            grid: true,
            overlay: Overlay::None,
            message: None,
            check: Check::default(),
            generation: 0,
            last_edit: Instant::now() - CHECK_DELAY,
            needs_check: false,
            redraw: true,
            quit: false,
        };
        editor.clamp_cursor();
        editor.needs_check = editor.structural.errors.is_empty();
        if is_new {
            editor.flash(format!("new level; s saves {}", path.display()));
        } else if let Some(warning) =
            editor.structural.warnings.first().cloned()
        {
            editor.flash(warning);
        }
        Ok(editor)
    }

    fn clamp_cursor(&mut self) {
        let width = self.doc.width().max(1);
        let height = self.doc.height().max(1);
        self.cursor.0 = self.cursor.0.min(width - 1);
        self.cursor.1 = self.cursor.1.min(height - 1);
    }

    fn move_cursor(&mut self, dx: isize, dy: isize) {
        let width = self.doc.width().max(1) as isize;
        let height = self.doc.height().max(1) as isize;
        self.cursor.0 =
            (self.cursor.0 as isize + dx).clamp(0, width - 1) as usize;
        self.cursor.1 =
            (self.cursor.1 as isize + dy).clamp(0, height - 1) as usize;
    }

    fn flash(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now() + MESSAGE_TIME));
        self.redraw = true;
    }

    fn paint(&mut self, erase: bool) {
        let (x, y) = self.cursor;
        let glyph = if erase { '.' } else { self.brush.glyph() };
        if paint_cell(&mut self.doc, x, y, glyph) {
            self.edited();
        }
    }

    fn edited(&mut self) {
        self.dirty = true;
        self.redraw = true;
        self.generation += 1;
        self.check.report = None;
        self.structural = structural_report(&self.doc, self.warnings.clone());
        self.needs_check = self.structural.errors.is_empty();
        self.last_edit = Instant::now();
    }

    fn metadata_changed(&mut self) {
        self.dirty = true;
        self.redraw = true;
        self.structural = structural_report(&self.doc, self.warnings.clone());
        if self.structural.errors.is_empty()
            && self.check.report.is_none()
            && !self.check.running
        {
            self.needs_check = true;
            self.last_edit = Instant::now();
        }
    }

    fn set_music(&mut self, name: &str) {
        if !MUSIC_NAMES.contains(&name) {
            self.flash(format!("unknown music '{name}'"));
            return;
        }
        if self.doc.meta_value("music") == Some(name) {
            return;
        }
        self.doc.set_meta("music", name);
        self.metadata_changed();
        self.flash(format!("music: {name}"));
    }

    fn set_background(&mut self, name: &str) {
        if !BACKGROUNDS.contains(&name) {
            self.flash(format!("unknown background '{name}'"));
            return;
        }
        if self.doc.meta_value("background") == Some(name) {
            return;
        }
        self.doc.set_meta("background", name);
        self.metadata_changed();
        self.flash(format!("background: {name}"));
    }

    fn save(&mut self) {
        match fs::write(&self.path, self.doc.serialize()) {
            Ok(()) => {
                self.dirty = false;
                self.warnings.clear();
                self.structural = structural_report(&self.doc, Vec::new());
                self.flash(format!("saved {}", self.path.display()));
            }
            Err(error) => self.flash(format!("save failed: {error}")),
        }
    }

    fn reload(&mut self) {
        if self.dirty {
            self.flash("unsaved changes; save first (s)");
            return;
        }
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) => {
                self.flash(format!("reload failed: {error}"));
                return;
            }
        };
        let (doc, warnings) = match parse_doc(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.flash(format!("reload failed: {error:#}"));
                return;
            }
        };
        self.doc = doc;
        self.warnings = warnings;
        self.clamp_cursor();
        self.structural = structural_report(&self.doc, self.warnings.clone());
        self.generation += 1;
        self.check.report = None;
        self.needs_check = self.structural.errors.is_empty();
        self.last_edit = Instant::now() - CHECK_DELAY;
        match self.structural.warnings.first().cloned() {
            Some(warning) => self.flash(warning),
            None => self.flash(format!("reloaded {}", self.path.display())),
        }
    }

    fn apply_size(&mut self, new: bool, width: usize, height: usize) {
        let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);

        if new {
            let name = self
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("New Level");
            self.doc = blank_doc(name, width, height);
            self.warnings.clear();
        } else {
            match resize_doc(&self.doc, width, height) {
                Ok(doc) => {
                    self.doc = doc;
                    self.warnings.clear();
                }
                Err(reason) => {
                    self.flash(reason);
                    return;
                }
            }
        }

        self.clamp_cursor();
        self.edited();
        self.flash(format!(
            "{} {}x{}",
            if new { "new level" } else { "resized to" },
            width,
            height
        ));
    }

    fn open_music(&mut self) {
        let current = self.doc.meta_value("music").unwrap_or(DEFAULT_MUSIC);
        let selected =
            MUSIC_NAMES.iter().position(|name| *name == current).unwrap_or(0);
        self.overlay = Overlay::Music(selected);
    }

    fn open_background(&mut self) {
        let current =
            self.doc.meta_value("background").unwrap_or(DEFAULT_BACKGROUND);
        let selected =
            BACKGROUNDS.iter().position(|name| *name == current).unwrap_or(0);
        self.overlay = Overlay::Background(selected);
    }

    fn open_size(&mut self, new: bool) {
        let (width, height) =
            if new { (40, 14) } else { (self.doc.width(), self.doc.height()) };
        self.overlay = Overlay::Size(SizePrompt {
            new,
            width: width.to_string(),
            height: height.to_string(),
            field: Field::Width,
        });
    }

    fn start_check(&mut self) {
        let level = self.doc.to_definition();
        let generation = self.generation;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let report = validator::validate(&level);
            let _ = sender.send((generation, report));
        });
        self.check.result = Some(receiver);
        self.check.running = true;
        self.needs_check = false;
        self.redraw = true;
    }

    fn tick(&mut self) {
        if let Some((_, until)) = &self.message
            && Instant::now() >= *until
        {
            self.message = None;
            self.redraw = true;
        }

        if self.check.running {
            let mut received = None;
            let mut disconnected = false;
            if let Some(receiver) = &self.check.result {
                match receiver.try_recv() {
                    Ok(value) => received = Some(value),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => disconnected = true,
                }
            }
            if disconnected {
                self.check.result = None;
                self.check.running = false;
            }
            if let Some((generation, report)) = received {
                self.check.result = None;
                self.check.running = false;
                if generation == self.generation {
                    self.check.report = Some(report);
                }
                self.redraw = true;
            }
        }

        if self.needs_check
            && !self.check.running
            && self.last_edit.elapsed() >= CHECK_DELAY
        {
            if self.structural.errors.is_empty() {
                self.start_check();
            } else {
                self.needs_check = false;
            }
        }
    }

    fn key(&mut self, key: KeyEvent) {
        self.redraw = true;
        if matches!(&self.overlay, Overlay::None) {
            self.normal_key(key);
        } else {
            self.overlay_key(key);
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let press = key.kind == KeyEventKind::Press;
        let step =
            if key.modifiers.contains(KeyModifiers::SHIFT) { 8 } else { 1 };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(0, -step),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(0, step),
            KeyCode::Left | KeyCode::Char('h') => self.move_cursor(-step, 0),
            KeyCode::Right | KeyCode::Char('l') => self.move_cursor(step, 0),
            KeyCode::Char(' ') | KeyCode::Enter => self.paint(false),
            KeyCode::Char('x') => self.paint(true),
            KeyCode::Char(glyph @ '1'..='6') if press => {
                self.brush = Brush::from_index((glyph as u8 - b'1') as usize);
                self.flash(format!("brush: {}", self.brush.label()));
            }
            KeyCode::Char('g') if press => self.grid = !self.grid,
            KeyCode::Char('v') if press => {
                self.needs_check = true;
                self.last_edit = Instant::now() - CHECK_DELAY;
            }
            KeyCode::Char('s') if press => self.save(),
            KeyCode::Char('r') if press => self.reload(),
            KeyCode::Char('m') if press => self.open_music(),
            KeyCode::Char('b') if press => self.open_background(),
            KeyCode::Char('n') if press => self.open_size(true),
            KeyCode::Char('z') if press => self.open_size(false),
            KeyCode::Char(':') if press => {
                self.overlay = Overlay::Command(String::new());
            }
            KeyCode::Char('?') if press => self.overlay = Overlay::Help,
            KeyCode::Char('q') | KeyCode::Esc if press => {
                if self.dirty {
                    self.overlay = Overlay::ConfirmQuit;
                } else {
                    self.quit = true;
                }
            }
            _ => {}
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        let press = key.kind == KeyEventKind::Press;
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);

        match overlay {
            Overlay::None => {}
            Overlay::Help => {
                if !press {
                    self.overlay = Overlay::Help;
                }
            }
            Overlay::ConfirmQuit => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if press => {
                    self.quit = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc
                    if press => {}
                _ => self.overlay = Overlay::ConfirmQuit,
            },
            Overlay::Music(mut selected) => {
                let len = MUSIC_NAMES.len();
                let mut close = false;
                match key.code {
                    KeyCode::Left => {
                        selected = (selected + len - 1) % len;
                    }
                    KeyCode::Right => {
                        selected = (selected + 1) % len;
                    }
                    KeyCode::Char(glyph @ '1'..='9') if press => {
                        let index = (glyph as u8 - b'1') as usize;
                        if index < len {
                            selected = index;
                        }
                    }
                    KeyCode::Enter if press => {
                        let name = MUSIC_NAMES[selected].to_string();
                        close = true;
                        self.set_music(&name);
                    }
                    KeyCode::Esc if press => close = true,
                    _ => {}
                }
                if !close {
                    self.overlay = Overlay::Music(selected);
                }
            }
            Overlay::Background(mut selected) => {
                let len = BACKGROUNDS.len();
                let mut close = false;
                match key.code {
                    KeyCode::Left => {
                        selected = (selected + len - 1) % len;
                    }
                    KeyCode::Right => {
                        selected = (selected + 1) % len;
                    }
                    KeyCode::Char(glyph @ '1'..='9') if press => {
                        let index = (glyph as u8 - b'1') as usize;
                        if index < len {
                            selected = index;
                        }
                    }
                    KeyCode::Enter if press => {
                        let name = BACKGROUNDS[selected].to_string();
                        close = true;
                        self.set_background(&name);
                    }
                    KeyCode::Esc if press => close = true,
                    _ => {}
                }
                if !close {
                    self.overlay = Overlay::Background(selected);
                }
            }
            Overlay::Size(mut prompt) => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc if press => close = true,
                    KeyCode::Tab => prompt.field = prompt.field.next(),
                    KeyCode::Backspace => {
                        prompt.field_mut().pop();
                    }
                    KeyCode::Char(glyph @ '0'..='9') => {
                        let field = prompt.field_mut();
                        if field.len() < 3 {
                            field.push(glyph);
                        }
                    }
                    KeyCode::Enter if press => {
                        let width = prompt.width.parse::<usize>().unwrap_or(0);
                        let height =
                            prompt.height.parse::<usize>().unwrap_or(0);
                        if width == 0 || height == 0 {
                            self.flash("width and height must be numbers");
                        } else {
                            close = true;
                            self.apply_size(prompt.new, width, height);
                        }
                    }
                    _ => {}
                }
                if !close {
                    self.overlay = Overlay::Size(prompt);
                }
            }
            Overlay::Command(mut input) => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc if press => close = true,
                    KeyCode::Enter if press => {
                        close = true;
                        self.run_command(&input);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(glyph)
                        if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        input.push(glyph);
                    }
                    _ => {}
                }
                if !close {
                    self.overlay = Overlay::Command(input);
                }
            }
        }
    }

    fn run_command(&mut self, input: &str) {
        match parse_command(input) {
            Ok(command) => self.apply_command(command),
            Err(reason) => self.flash(reason),
        }
    }

    fn apply_command(&mut self, command: Command) {
        match command {
            Command::Brush(glyph) => match Brush::from_char(glyph) {
                Some(brush) => {
                    self.brush = brush;
                    self.flash(format!("brush: {}", brush.label()));
                }
                None => self.flash(format!("unknown brush '{glyph}'")),
            },
            Command::Music(name) => self.set_music(&name),
            Command::Background(name) => self.set_background(&name),
            Command::New(width, height) => {
                self.apply_size(true, width, height);
            }
            Command::Resize(width, height) => {
                self.apply_size(false, width, height);
            }
            Command::Name(name) => {
                self.doc.set_meta("name", &name);
                self.metadata_changed();
                self.flash(format!("name: {name}"));
            }
            Command::Save => self.save(),
            Command::Reload => self.reload(),
            Command::Check => {
                self.needs_check = true;
                self.last_edit = Instant::now() - CHECK_DELAY;
            }
            Command::Grid(on) => self.grid = on,
            Command::Goto(x, y) => {
                self.cursor = (x, y);
                self.clamp_cursor();
            }
            Command::Help => self.overlay = Overlay::Help,
            Command::Quit => {
                if self.dirty {
                    self.overlay = Overlay::ConfirmQuit;
                } else {
                    self.quit = true;
                }
            }
        }
    }

    fn camera(&self, level: &LevelDefinition) -> Camera {
        let tile = TILE_SIZE as f32;
        let level_width = level.width as f32 * tile;
        let level_height = level.height as f32 * tile;
        let view_width = VIEW_WIDTH as f32;
        let view_height = VIEW_HEIGHT as f32;
        Camera {
            x: (self.cursor.0 as f32 * tile + tile / 2.0 - view_width / 2.0)
                .clamp(0.0, (level_width - view_width).max(0.0)),
            y: (self.cursor.1 as f32 * tile + tile / 2.0 - view_height / 2.0)
                .clamp(0.0, (level_height - view_height).max(0.0)),
        }
    }

    fn frame(&self) -> Result<RgbaImage> {
        let mut level = self.doc.to_definition();
        if let Overlay::Background(selected) = &self.overlay {
            level.background = BACKGROUNDS[*selected].to_string();
        }
        let camera = self.camera(&level);
        let mut frame = render::level(
            &level,
            None,
            &camera,
            &level.coins,
            0.0,
            VIEW_WIDTH,
            VIEW_HEIGHT,
        )?;
        self.draw_overlays(&mut frame, &level, &camera);
        Ok(frame)
    }

    fn draw_overlays(
        &self,
        frame: &mut RgbaImage,
        level: &LevelDefinition,
        camera: &Camera,
    ) {
        let tile = TILE_SIZE as i32;
        let size = TILE_SIZE;

        if let Some(report) = &self.check.report
            && report.reached.len() == level.width * level.height
        {
            for y in 0..level.height {
                for x in 0..level.width {
                    if !report.reached[y * level.width + x] {
                        continue;
                    }
                    let (sx, sy) = camera
                        .world_to_screen((x * size) as f32, (y * size) as f32);
                    fill(
                        frame,
                        sx,
                        sy + tile - 3,
                        tile,
                        3,
                        [127, 215, 255, 40],
                    );
                }
            }
        }

        for (y, row) in self.doc.rows.iter().enumerate() {
            for (x, &glyph) in row.iter().enumerate() {
                if glyph != 'P' {
                    continue;
                }
                let (sx, sy) = camera
                    .world_to_screen((x * size) as f32, (y * size) as f32);
                fill(frame, sx, sy, tile, 2, [80, 140, 220, 120]);
            }
        }

        if self.grid {
            let color = [255, 255, 255, 26];
            let first_x = (camera.x / tile as f32).floor() as i32;
            let last_x =
                ((camera.x + VIEW_WIDTH as f32) / tile as f32).ceil() as i32;
            for x in first_x..=last_x {
                let sx = (x as f32 * tile as f32 - camera.x).round() as i32;
                fill(frame, sx, 0, 1, VIEW_HEIGHT as i32, color);
            }
            let first_y = (camera.y / tile as f32).floor() as i32;
            let last_y =
                ((camera.y + VIEW_HEIGHT as f32) / tile as f32).ceil() as i32;
            for y in first_y..=last_y {
                let sy = (y as f32 * tile as f32 - camera.y).round() as i32;
                fill(frame, 0, sy, VIEW_WIDTH as i32, 1, color);
            }
        }

        let red = [255, 90, 90, 255];
        for &(x, y) in &self.structural.marks {
            let (sx, sy) =
                camera.world_to_screen((x * size) as f32, (y * size) as f32);
            outline(frame, sx, sy, tile, tile, red);
        }

        if let Some(report) = &self.check.report {
            for (index, &(x, y)) in level.coins.iter().enumerate() {
                if report.coin_hit.get(index).copied().unwrap_or(true) {
                    continue;
                }
                let (sx, sy) = camera
                    .world_to_screen((x * size) as f32, (y * size) as f32);
                cross(frame, sx, sy, tile, red);
            }
        }

        if let Some((x, y)) = self.doc.spawn() {
            let (sx, sy) =
                camera.world_to_screen((x * size) as f32, (y * size) as f32);
            outline(frame, sx, sy, tile, tile, [102, 255, 153, 230]);
        }
        if let Some((x, y)) = self.doc.goal() {
            let (sx, sy) =
                camera.world_to_screen((x * size) as f32, (y * size) as f32);
            outline(frame, sx, sy, tile, tile, [255, 111, 174, 230]);
        }

        let (cx, cy) = camera.world_to_screen(
            (self.cursor.0 * size) as f32,
            (self.cursor.1 * size) as f32,
        );
        outline(
            frame,
            cx - 1,
            cy - 1,
            tile + 2,
            tile + 2,
            [255, 255, 255, 255],
        );

        let failed = !self.structural.errors.is_empty()
            || self
                .check
                .report
                .as_ref()
                .is_some_and(|report| report.failure().is_some());
        if failed {
            border(frame, red);
        }
    }

    fn verdict(&self) -> String {
        if let Some(error) = self.structural.errors.first() {
            let extra = self.structural.errors.len() - 1;
            return if extra == 0 {
                format!("invalid: {error}")
            } else {
                format!("invalid: {error} (+{extra} more)")
            };
        }
        if self.check.running || self.needs_check {
            return "checking".to_string();
        }
        match &self.check.report {
            Some(report) => {
                report.failure().unwrap_or_else(|| "playable".to_string())
            }
            None => "unchecked".to_string(),
        }
    }

    fn status_line(&self) -> String {
        let coins = self.doc.coins().len();
        format!(
            " {}  {},{}  {} coin{}  {}  {}x{}  music:{}  bg:{}  {}",
            self.brush.label(),
            self.cursor.0,
            self.cursor.1,
            coins,
            if coins == 1 { "" } else { "s" },
            self.verdict(),
            self.doc.width(),
            self.doc.height(),
            self.doc.meta_value("music").unwrap_or("?"),
            self.doc.meta_value("background").unwrap_or("?"),
            if self.dirty { "*" } else { "" },
        )
    }

    fn keys_line(&self) -> String {
        " 1-6 brushes  arrows/hjkl move  Shift+arrow x8  space paint  x erase  g grid  v check  s save  r reload  m music  b bg  n new  z resize  : cmd  ? help  q quit".to_string()
    }

    fn hud_lines(&self) -> Vec<String> {
        let mut lines = vec![self.status_line()];

        match &self.overlay {
            Overlay::None => {
                lines.push(self.keys_line());
                if let Some((message, _)) = &self.message {
                    lines.push(format!(" {message}"));
                }
            }
            Overlay::Music(selected) => {
                lines.push(music_line(*selected));
            }
            Overlay::Background(selected) => {
                lines.push(background_line(*selected));
            }
            Overlay::Size(prompt) => lines.push(prompt.line()),
            Overlay::Command(input) => {
                lines.push(format!(" :{input}_"));
            }
            Overlay::Help => {
                lines.push(
                    " 1-6 brushes  arrows/hjkl move  Shift+arrow x8  space paint  x erase  g grid  v check".to_string(),
                );
                lines.push(
                    " s save  r reload  m music  b background  n new  z resize  : command  ? help  q quit".to_string(),
                );
            }
            Overlay::ConfirmQuit => {
                lines.push(" unsaved changes; y discard, n cancel".to_string());
            }
        }

        lines
    }

    fn draw(&self, term: &Terminal) -> Result<()> {
        let (columns, rows) = term.size();
        let placement =
            term.image_placement(VIEW_WIDTH as usize, VIEW_HEIGHT as usize);

        let below = placement.row + placement.height < rows;
        let hud_column = if below { 0 } else { placement.column };
        let hud_row = if below {
            placement.row + placement.height
        } else {
            placement.row.min(rows.saturating_sub(HUD_LINES))
        };

        let mut stdout = io::stdout().lock();
        for line in 0..HUD_LINES {
            let row = hud_row + line;
            if row < rows {
                queue!(
                    stdout,
                    cursor::MoveTo(hud_column, row),
                    Clear(ClearType::CurrentLine)
                )?;
            }
        }
        stdout.flush()?;
        drop(stdout);

        let frame = self.frame()?;
        kitty_graphics::display_rgba(
            frame.as_raw(),
            VIEW_WIDTH as usize,
            VIEW_HEIGHT as usize,
            placement,
        )?;

        let lines = self.hud_lines();
        let mut stdout = io::stdout().lock();
        for (index, line) in lines.iter().take(HUD_LINES as usize).enumerate() {
            let row = hud_row + index as u16;
            if row >= rows {
                break;
            }
            let width = columns.saturating_sub(1) as usize;
            let text: String = line.chars().take(width).collect();
            queue!(stdout, cursor::MoveTo(hud_column, row))?;
            write!(stdout, "\x1b[40m{text}\x1b[K\x1b[0m")?;
        }
        stdout.flush()?;

        Ok(())
    }
}

fn music_line(selected: usize) -> String {
    let mut line = String::from(" music > ");
    for (index, name) in MUSIC_NAMES.iter().enumerate() {
        if index == selected {
            line.push_str(&format!("[{}] {}   ", index + 1, name));
        } else {
            line.push_str(&format!("{} {}   ", index + 1, name));
        }
    }
    line.push_str("(1-4, Enter, Esc)");
    line
}

fn background_line(selected: usize) -> String {
    let mut line = String::from(" background > ");
    for (index, name) in BACKGROUNDS.iter().enumerate() {
        if index == selected {
            line.push_str(&format!("[{}] {}   ", index + 1, name));
        } else {
            line.push_str(&format!("{} {}   ", index + 1, name));
        }
    }
    line.push_str("(1-3, Enter, Esc)");
    line
}

fn blend(frame: &mut RgbaImage, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= frame.width() as i32 || y >= frame.height() as i32
    {
        return;
    }
    let pixel = frame.get_pixel_mut(x as u32, y as u32);
    let alpha = color[3] as u32;
    for (channel, value) in color.iter().enumerate().take(3) {
        pixel.0[channel] = ((pixel.0[channel] as u32 * (255 - alpha)
            + *value as u32 * alpha)
            / 255) as u8;
    }
    pixel.0[3] = 255;
}

fn fill(
    frame: &mut RgbaImage,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color: [u8; 4],
) {
    for dy in 0..height {
        for dx in 0..width {
            blend(frame, x + dx, y + dy, color);
        }
    }
}

fn outline(
    frame: &mut RgbaImage,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color: [u8; 4],
) {
    fill(frame, x, y, width, 2, color);
    fill(frame, x, y + height - 2, width, 2, color);
    fill(frame, x, y, 2, height, color);
    fill(frame, x + width - 2, y, 2, height, color);
}

fn cross(frame: &mut RgbaImage, x: i32, y: i32, size: i32, color: [u8; 4]) {
    for i in 0..size {
        blend(frame, x + i, y + i, color);
        blend(frame, x + i + 1, y + i, color);
        blend(frame, x + size - 1 - i, y + i, color);
        blend(frame, x + size - i, y + i, color);
    }
}

fn border(frame: &mut RgbaImage, color: [u8; 4]) {
    let width = frame.width() as i32;
    let height = frame.height() as i32;
    fill(frame, 0, 0, width, 2, color);
    fill(frame, 0, height - 2, width, 2, color);
    fill(frame, 0, 0, 2, height, color);
    fill(frame, width - 2, 0, 2, height, color);
}

pub fn run(path: &Path) -> Result<()> {
    let mut editor = Editor::open(path)?;
    let mut term = Terminal::init()?;
    let mut last_draw = Instant::now() - Duration::from_secs(1);

    loop {
        let events = term.read_events()?;
        for key in events {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            editor.key(key);
        }

        editor.tick();

        if editor.redraw || last_draw.elapsed() >= Duration::from_millis(100) {
            editor.draw(&term)?;
            editor.redraw = false;
            last_draw = Instant::now();
        }

        if editor.quit {
            break;
        }

        thread::sleep(Duration::from_millis(16));
    }

    term.deinit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = "name: t\nmusic: colors\nbackground: snow\n\
                        palette: #000000, #111111, #222222, #333333\n\n";

    fn doc(rows: &str) -> LevelDoc {
        parse_doc(&format!("{META}{rows}")).expect("the level parses").0
    }

    #[test]
    fn serialization_round_trips() {
        let rows = "S.F\nGGG\n";
        assert_eq!(doc(rows).serialize(), format!("{META}{rows}"));
    }

    #[test]
    fn ragged_rows_are_padded_with_air() {
        let (parsed, warnings) =
            parse_doc(&format!("{META}S.F\nGG\n")).expect("the level parses");
        assert_eq!(parsed.width(), 3);
        assert_eq!(parsed.rows[1], vec!['G', 'G', '.']);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn painting_the_spawn_moves_it() {
        let mut parsed = doc("S...F\nGGGGG\n");
        assert!(paint_cell(&mut parsed, 3, 0, 'S'));
        assert_eq!(parsed.spawns(), vec![(3, 0)]);
        assert!(!paint_cell(&mut parsed, 3, 0, 'S'));
    }

    #[test]
    fn resizing_refuses_to_cut_off_items() {
        let parsed = blank_doc("test", 20, 10);
        let error = resize_doc(&parsed, 17, 10).expect_err("cuts the goal");
        assert!(error.contains("goal"), "{error}");
        let resized = resize_doc(&parsed, 20, 10).expect("fits");
        assert_eq!(resized.width(), 20);
    }

    #[test]
    fn resizing_pads_with_air() {
        let parsed = blank_doc("test", 16, 8);
        let resized = resize_doc(&parsed, 20, 12).expect("grows");
        assert_eq!(resized.width(), 20);
        assert_eq!(resized.height(), 12);
        assert_eq!(resized.rows[11], vec!['.'; 20]);
    }

    #[test]
    fn a_blank_level_has_a_spawn_and_a_goal() {
        let parsed = blank_doc("test", 40, 14);
        assert_eq!(parsed.spawns().len(), 1);
        assert_eq!(parsed.goals().len(), 1);
        assert_eq!(parsed.rows[13][0], 'G');
    }

    #[test]
    fn structural_errors_are_reported() {
        let (parsed, warnings) =
            parse_doc(&format!("{META}.F\nG\n")).expect("the level parses");
        let report = structural_report(&parsed, warnings);
        assert!(report.errors.iter().any(|error| error.contains("no spawn")));
        assert!(
            report.warnings.iter().any(|warning| warning.contains("1 wide"))
        );

        let report = structural_report(&doc("SS.F\nGGGG\n"), Vec::new());
        assert!(report.errors.iter().any(|error| error.contains("2 spawns")));

        let report = structural_report(&doc("S?F\nGGG\n"), Vec::new());
        assert!(
            report.errors.iter().any(|error| error.contains("unknown tile"))
        );
    }

    #[test]
    fn a_loaded_level_renders() {
        let editor = Editor::open(Path::new("levels/level1.txt"))
            .expect("the level opens");
        let frame = editor.frame().expect("the level renders");
        assert_eq!(frame.width(), VIEW_WIDTH);
        assert_eq!(frame.height(), VIEW_HEIGHT);
    }

    #[test]
    fn the_hud_names_the_brush_and_level() {
        let editor = Editor::open(Path::new("levels/level1.txt"))
            .expect("the level opens");
        let line = editor.status_line();
        assert!(line.contains("ground"), "{line}");
        assert!(line.contains("72x17"), "{line}");
        assert!(line.contains("music:echoes-of-solace"), "{line}");
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command("new 40 14"), Ok(Command::New(40, 14)));
        assert_eq!(parse_command("resize 72x17"), Ok(Command::Resize(72, 17)));
        assert_eq!(
            parse_command("music colors"),
            Ok(Command::Music("colors".to_string()))
        );
        assert_eq!(parse_command("grid off"), Ok(Command::Grid(false)));
        assert!(parse_command("nope").is_err());
    }
}
