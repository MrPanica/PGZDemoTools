#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use pgz_demo_tools::demo::{DemoInfo, DemoKind, read_demo, safe_name};
use pgz_demo_tools::edit::{edit_demo_with_freecam_progress, edit_demo_with_progress};
use pgz_demo_tools::voice::{DemoEvent, DemoIndex, create_zip, export_voices};
use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Page {
    Demo,
    Voices,
    Events,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Language {
    System,
    Russian,
    English,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Theme {
    System,
    Light,
    Dark,
}

enum Message {
    Loaded(Result<(Arc<DemoInfo>, Vec<usize>), String>),
    Indexed(Result<DemoIndex, String>),
    Edited(Result<PathBuf, String>),
    Voices(Result<Vec<PathBuf>, String>),
}

struct Busy {
    label: &'static str,
    progress: Arc<AtomicU8>,
}

struct DesktopApp {
    workspace: PathBuf,
    info: Option<Arc<DemoInfo>>,
    density: Vec<usize>,
    index: Option<DemoIndex>,
    ranges: Vec<(u32, u32)>,
    start_seconds: f64,
    end_seconds: f64,
    output_name: String,
    unlock_camera: bool,
    selected_voices: BTreeSet<u8>,
    keep_gaps: bool,
    archive_voices: bool,
    audio_format: String,
    page: Page,
    language: Language,
    theme: Theme,
    show_settings: bool,
    status: String,
    busy: Option<Busy>,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
}

impl DesktopApp {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            workspace: default_workspace(),
            info: None,
            density: Vec::new(),
            index: None,
            ranges: Vec::new(),
            start_seconds: 0.0,
            end_seconds: 0.0,
            output_name: String::new(),
            unlock_camera: false,
            selected_voices: BTreeSet::new(),
            keep_gaps: true,
            archive_voices: false,
            audio_format: "ogg".to_owned(),
            page: Page::Demo,
            language: Language::System,
            theme: Theme::System,
            show_settings: false,
            status: "Откройте TF2-демку.".to_owned(),
            busy: None,
            sender,
            receiver,
        }
    }

    fn russian(&self) -> bool {
        match self.language {
            Language::Russian => true,
            Language::English => false,
            Language::System => sys_locale::get_locale()
                .is_some_and(|locale| locale.to_ascii_lowercase().starts_with("ru")),
        }
    }

    fn text<'a>(&self, russian: &'a str, english: &'a str) -> &'a str {
        if self.russian() { russian } else { english }
    }

    fn set_status(&mut self, value: impl Into<String>) {
        self.status = value.into();
    }

    fn open_demo(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TF2 demo", &["dem"])
            .pick_file()
        else {
            return;
        };
        self.set_status(self.text("Читаю демку…", "Reading demo…"));
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = read_demo(&path)
                .map(|info| {
                    let info = Arc::new(info);
                    let density = info.meta().density;
                    (info, density)
                })
                .map_err(|error| format!("{error:?}"));
            let _ = sender.send(Message::Loaded(result));
        });
    }

    fn load_index(&mut self) {
        let Some(info) = self.info.clone() else {
            return;
        };
        let cache = self.workspace.join("desktop-index").join(safe_name(
            &info.path.file_stem().unwrap_or_default().to_string_lossy(),
            "demo",
        ));
        self.set_status(self.text("Ищу игроков и события…", "Reading players and events…"));
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = pgz_demo_tools::voice::extract_demo_index(&info, &cache)
                .map_err(|error| format!("{error:?}"));
            let _ = sender.send(Message::Indexed(result));
        });
    }

    fn add_range(&mut self) {
        let Some(info) = &self.info else { return };
        let start = (self.start_seconds.max(0.0) * info.tick_rate).round() as u32;
        let end = (self.end_seconds.max(0.0) * info.tick_rate).round() as u32;
        if start >= end || end > info.ticks {
            self.set_status(self.text("Некорректный диапазон.", "Invalid range."));
            return;
        }
        self.ranges.push((start, end));
    }

    fn start_edit(&mut self) {
        let Some(info) = self.info.clone() else {
            return;
        };
        if self.ranges.is_empty() {
            self.set_status(self.text("Добавьте хотя бы один отрезок.", "Add at least one clip."));
            return;
        }
        let Some(target) = rfd::FileDialog::new()
            .set_file_name(format!(
                "{}.dem",
                default_output_name(&self.output_name, &info)
            ))
            .add_filter("TF2 demo", &["dem"])
            .save_file()
        else {
            return;
        };
        let progress = Arc::new(AtomicU8::new(0));
        self.busy = Some(Busy {
            label: if self.unlock_camera {
                "Free camera"
            } else {
                "Montage"
            },
            progress: Arc::clone(&progress),
        });
        self.set_status(self.text("Монтирую демку…", "Editing demo…"));
        let sender = self.sender.clone();
        let workspace = self.workspace.clone();
        let ranges = self.ranges.clone();
        let unlock_camera = self.unlock_camera;
        thread::spawn(move || {
            let mut report = |value: u8| progress.store(value, Ordering::Relaxed);
            let result = if unlock_camera {
                edit_demo_with_freecam_progress(&info, &ranges, &target, &workspace, &mut report)
            } else {
                edit_demo_with_progress(&info, &ranges, &target, &workspace, &mut report)
            }
            .map_err(|error| format!("{error:?}"));
            let _ = sender.send(Message::Edited(result));
        });
    }

    fn export_voices(&mut self) {
        let Some(info) = self.info.clone() else {
            return;
        };
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if self.selected_voices.is_empty() {
            self.set_status(self.text("Выберите голосовые дорожки.", "Select voice tracks."));
            return;
        }
        let selected = self
            .selected_voices
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>();
        let progress = Arc::new(AtomicU8::new(0));
        self.busy = Some(Busy {
            label: "Audio",
            progress: Arc::clone(&progress),
        });
        self.set_status(self.text("Экспортирую голоса…", "Exporting voices…"));
        let sender = self.sender.clone();
        let workspace = self.workspace.clone();
        let format = self.audio_format.clone();
        let keep_gaps = self.keep_gaps;
        let archive_voices = self.archive_voices;
        thread::spawn(move || {
            progress.store(10, Ordering::Relaxed);
            let result = export_voices(
                &info, &folder, &selected, false, keep_gaps, &format, &workspace,
            )
            .and_then(|paths| {
                if archive_voices && paths.len() > 1 {
                    create_zip(&paths, &folder.join("voices.zip"))?;
                }
                Ok(paths)
            })
            .map_err(|error| format!("{error:?}"));
            progress.store(100, Ordering::Relaxed);
            let _ = sender.send(Message::Voices(result));
        });
    }

    fn poll_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Loaded(Ok((info, density))) => {
                    self.end_seconds = info.duration;
                    self.output_name = info
                        .path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    self.info = Some(info);
                    self.density = density;
                    self.index = None;
                    self.ranges.clear();
                    self.selected_voices.clear();
                    self.set_status(self.text("Демка готова к монтажу.", "Demo is ready."));
                    self.load_index();
                }
                Message::Loaded(Err(error)) => self.set_status(error),
                Message::Indexed(Ok(index)) => {
                    self.index = Some(index);
                    self.set_status(
                        self.text("Игроки и события загружены.", "Players and events loaded."),
                    );
                }
                Message::Indexed(Err(error)) => self.set_status(error),
                Message::Edited(Ok(path)) => {
                    self.busy = None;
                    self.set_status(format!(
                        "{} {}",
                        self.text("Готово:", "Ready:"),
                        path.display()
                    ));
                }
                Message::Edited(Err(error)) => {
                    self.busy = None;
                    self.set_status(error);
                }
                Message::Voices(Ok(paths)) => {
                    self.busy = None;
                    self.set_status(format!(
                        "{} {}",
                        self.text("Создано дорожек:", "Tracks created:"),
                        paths.len()
                    ));
                }
                Message::Voices(Err(error)) => {
                    self.busy = None;
                    self.set_status(error);
                }
            }
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        match self.theme {
            Theme::Light => ctx.set_visuals(egui::Visuals::light()),
            Theme::Dark => ctx.set_visuals(egui::Visuals::dark()),
            Theme::System => {}
        }
    }

    fn show_menu(&mut self, ctx: &egui::Context) {
        let mut open_demo = false;
        let mut open_settings = false;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button(self.text("Файл", "File"), |ui| {
                    if ui
                        .button(self.text("Открыть .dem…", "Open .dem…"))
                        .clicked()
                    {
                        open_demo = true;
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(
                            self.info.is_some(),
                            egui::Button::new(self.text("Сохранить монтаж…", "Save montage…")),
                        )
                        .clicked()
                    {
                        self.start_edit();
                        ui.close_menu();
                    }
                    if ui.button(self.text("Выход", "Quit")).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button(self.text("Настройки", "Settings"), |ui| {
                    if ui
                        .button(self.text("Оформление и язык", "Appearance and language"))
                        .clicked()
                    {
                        open_settings = true;
                        ui.close_menu();
                    }
                });
                ui.separator();
                ui.label("TF2 DEMO TOOLS");
            });
        });
        if open_demo {
            self.open_demo();
        }
        if open_settings {
            self.show_settings = true;
        }
    }

    fn show_navigation(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("navigation")
            .resizable(false)
            .default_width(176.0)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.heading("TF2 DEMO TOOLS");
                ui.add_space(18.0);
                for (page, russian, english) in [
                    (Page::Demo, "Монтаж", "Montage"),
                    (Page::Voices, "Голоса", "Voices"),
                    (Page::Events, "События", "Events"),
                ] {
                    if ui
                        .selectable_label(self.page == page, self.text(russian, english))
                        .clicked()
                    {
                        self.page = page;
                    }
                }
                ui.separator();
                if ui.button(self.text("Открыть демку", "Open demo")).clicked() {
                    self.open_demo();
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if ui.button(self.text("Настройки", "Settings")).clicked() {
                        self.show_settings = true;
                    }
                });
            });
    }

    fn show_demo(&mut self, ui: &mut egui::Ui) {
        let Some(info) = self.info.clone() else {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading(self.text("Откройте TF2-демку", "Open a TF2 demo"));
                ui.label(self.text(
                    "POV и SourceTV обрабатываются локально теми же Rust-алгоритмами, что CLI и веб.",
                    "POV and SourceTV use the same local Rust algorithms as CLI and web.",
                ));
                ui.add_space(12.0);
                if ui.button(self.text("Выбрать .dem", "Choose .dem")).clicked() {
                    self.open_demo();
                }
            });
            return;
        };
        ui.heading(&self.output_name);
        ui.horizontal_wrapped(|ui| {
            badge(ui, info.kind.label(), Color32::from_rgb(239, 127, 69));
            badge(ui, &info.map, Color32::from_rgb(61, 111, 168));
            badge(
                ui,
                &format!("{:.2}s", info.duration),
                Color32::from_rgb(31, 143, 111),
            );
            badge(
                ui,
                &format!("{} ticks", info.ticks),
                Color32::from_rgb(132, 97, 201),
            );
        });
        ui.add_space(10.0);
        self.timeline(ui, &info);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(self.text("Начало, с", "Start, s"));
            ui.add(
                egui::DragValue::new(&mut self.start_seconds)
                    .speed(0.05)
                    .range(0.0..=info.duration),
            );
            ui.label(self.text("Конец, с", "End, s"));
            ui.add(
                egui::DragValue::new(&mut self.end_seconds)
                    .speed(0.05)
                    .range(0.0..=info.duration),
            );
            if ui.button(self.text("+ В монтаж", "+ Add clip")).clicked() {
                self.add_range();
            }
        });
        if info.kind == DemoKind::Pov {
            let label = self
                .text("Разблокировать свободную камеру", "Unlock free camera")
                .to_owned();
            ui.checkbox(
                &mut self.unlock_camera,
                label,
            )
            .on_hover_text(self.text(
                "Преобразует готовый POV-монтаж в SourceTV-наблюдателя со свободным полётом камеры.",
                "Converts the finished POV montage into a free-flying SourceTV observer.",
            ));
        }
        ui.add_space(8.0);
        ui.label(self.text("Порядок монтажа", "Montage order"));
        let mut remove = None;
        let mut move_up = None;
        let mut move_down = None;
        for (index, (start, end)) in self.ranges.iter().copied().enumerate() {
            ui.horizontal(|ui| {
                ui.monospace(format!(
                    "#{:02}  {:.2}s → {:.2}s",
                    index + 1,
                    start as f64 / info.tick_rate,
                    end as f64 / info.tick_rate
                ));
                if ui.small_button("↑").clicked() && index > 0 {
                    move_up = Some(index);
                }
                if ui.small_button("↓").clicked() && index + 1 < self.ranges.len() {
                    move_down = Some(index);
                }
                if ui.small_button("×").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = move_up {
            self.ranges.swap(index, index - 1);
        }
        if let Some(index) = move_down {
            self.ranges.swap(index, index + 1);
        }
        if let Some(index) = remove {
            self.ranges.remove(index);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(self.text("Имя файла", "Filename"));
            ui.text_edit_singleline(&mut self.output_name);
            if ui
                .add_enabled(
                    !self.ranges.is_empty() && self.busy.is_none(),
                    egui::Button::new(self.text("Сохранить монтаж…", "Save montage…")),
                )
                .clicked()
            {
                self.start_edit();
            }
        });
    }

    fn timeline(&self, ui: &mut egui::Ui, info: &DemoInfo) {
        let desired = Vec2::new(ui.available_width(), 150.0);
        let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 7.0, ui.visuals().faint_bg_color);
        let peak = self.density.iter().copied().max().unwrap_or(1).max(1) as f32;
        let width = rect.width() / self.density.len().max(1) as f32;
        for (index, value) in self.density.iter().copied().enumerate() {
            let height = (value as f32 / peak * (rect.height() - 18.0)).max(2.0);
            let x = rect.left() + index as f32 * width;
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(x, rect.bottom() - height),
                    Vec2::new(width.max(1.0), height),
                ),
                0.0,
                Color32::from_rgb(239, 127, 69),
            );
        }
        for (start, end) in &self.ranges {
            let left = rect.left() + *start as f32 / info.ticks as f32 * rect.width();
            let right = rect.left() + *end as f32 / info.ticks as f32 * rect.width();
            painter.rect_filled(
                Rect::from_x_y_ranges(left..=right, rect.top()..=rect.bottom()),
                0.0,
                Color32::from_rgba_premultiplied(103, 198, 154, 66),
            );
        }
        painter.text(
            rect.left_top() + Vec2::new(8.0, 7.0),
            Align2::LEFT_TOP,
            format!("0:00 · {:.2}", info.duration),
            FontId::monospace(11.0),
            ui.visuals().text_color(),
        );
    }

    fn show_voices(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.text("Голоса игроков", "Player voices"));
        let Some(index) = &self.index else {
            ui.label(self.text("Индексирую голосовые пакеты…", "Indexing voice packets…"));
            return;
        };
        let players = index.players.clone();
        ui.horizontal(|ui| {
            if ui.button(self.text("Выбрать всех", "Select all")).clicked() {
                self.selected_voices = players.iter().map(|player| player.client).collect();
            }
            if ui
                .button(self.text("Снять выбор", "Clear selection"))
                .clicked()
            {
                self.selected_voices.clear();
            }
        });
        egui::ScrollArea::vertical()
            .max_height(380.0)
            .show(ui, |ui| {
                for player in &players {
                    ui.horizontal(|ui| {
                        let mut selected = self.selected_voices.contains(&player.client);
                        if ui.checkbox(&mut selected, "").changed() {
                            if selected {
                                self.selected_voices.insert(player.client);
                            } else {
                                self.selected_voices.remove(&player.client);
                            }
                        }
                        ui.label(&player.name);
                        ui.monospace(format!("{} · {}", player.steamid, player.packets));
                    });
                }
            });
        ui.separator();
        let keep_gaps_label = self.text("Сохранить паузы", "Keep pauses").to_owned();
        ui.checkbox(&mut self.keep_gaps, keep_gaps_label);
        let archive_label = self
            .text(
                "Упаковать несколько дорожек в ZIP",
                "Archive multiple tracks as ZIP",
            )
            .to_owned();
        ui.checkbox(&mut self.archive_voices, archive_label);
        egui::ComboBox::from_label(self.text("Формат", "Format"))
            .selected_text(&self.audio_format)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.audio_format, "ogg".to_owned(), "OGG · Opus");
                ui.selectable_value(&mut self.audio_format, "wav".to_owned(), "WAV · PCM");
                ui.selectable_value(&mut self.audio_format, "mp3".to_owned(), "MP3 · 128 kbps");
            });
        if ui
            .add_enabled(
                !self.selected_voices.is_empty() && self.busy.is_none(),
                egui::Button::new(self.text("Экспортировать…", "Export…")),
            )
            .clicked()
        {
            self.export_voices();
        }
    }

    fn show_events(&self, ui: &mut egui::Ui) {
        ui.heading(self.text("События", "Events"));
        let Some(info) = &self.info else {
            return;
        };
        let Some(index) = &self.index else {
            ui.label(self.text("Индексирую события…", "Indexing events…"));
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            for event in &index.events {
                event_row(ui, event, info.tick_rate);
            }
        });
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        egui::Window::new(self.text("Настройки", "Settings"))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(self.text("Язык", "Language"));
                let system_language = self.text("Системный", "System").to_owned();
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.language, Language::System, system_language);
                    ui.selectable_value(&mut self.language, Language::Russian, "Русский");
                    ui.selectable_value(&mut self.language, Language::English, "English");
                });
                ui.separator();
                ui.label(self.text("Тема", "Theme"));
                let system_theme = self.text("Системная", "System").to_owned();
                let light_theme = self.text("Светлая", "Light").to_owned();
                let dark_theme = self.text("Тёмная", "Dark").to_owned();
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.theme, Theme::System, system_theme);
                    ui.selectable_value(&mut self.theme, Theme::Light, light_theme);
                    ui.selectable_value(&mut self.theme, Theme::Dark, dark_theme);
                });
                ui.separator();
                ui.small(format!(
                    "{} {}",
                    self.text("Рабочая папка:", "Workspace:"),
                    self.workspace.display()
                ));
            });
        self.show_settings = open;
        self.apply_theme(ctx);
    }

    fn show_busy(&self, ui: &mut egui::Ui) {
        if let Some(busy) = &self.busy {
            let progress = busy.progress.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(progress as f32 / 100.0)
                        .text(format!("{} · {progress}%", busy.label)),
                );
                ui.spinner();
            });
        }
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        self.apply_theme(ctx);
        self.show_menu(ctx);
        self.show_navigation(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.page {
            Page::Demo => self.show_demo(ui),
            Page::Voices => self.show_voices(ui),
            Page::Events => self.show_events(ui),
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            self.show_busy(ui);
            ui.label(&self.status);
        });
        self.show_settings(ctx);
        if self.busy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.22))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(7, 4))
        .show(ui, |ui| ui.label(text));
}

fn event_row(ui: &mut egui::Ui, event: &DemoEvent, tick_rate: f64) {
    ui.horizontal_wrapped(|ui| {
        ui.monospace(format!("{:>8.2}s", event.tick as f64 / tick_rate));
        ui.strong(&event.kind);
        ui.label(&event.actor);
        if !event.target.is_empty() {
            ui.label("→");
            ui.label(&event.target);
        }
        if !event.detail.is_empty() {
            ui.label(format!("· {}", event.detail));
        }
    });
}

fn default_output_name(name: &str, info: &DemoInfo) -> String {
    let value = name.trim().trim_end_matches(".dem");
    if value.is_empty() {
        format!(
            "{}-edit",
            info.path.file_stem().unwrap_or_default().to_string_lossy()
        )
    } else {
        value.to_owned()
    }
}

fn default_workspace() -> PathBuf {
    if let Some(path) = env::var_os("PGZ_DEMO_WORKSPACE") {
        return PathBuf::from(path);
    }
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".work")
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1240.0, 820.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PGZ Demo Tools",
        options,
        Box::new(|_creation_context| Ok(Box::new(DesktopApp::new()))),
    )
}
