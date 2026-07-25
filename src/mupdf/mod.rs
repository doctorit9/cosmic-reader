use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::SmolStr;
use cosmic::iced::event::{self, Event};
use cosmic::iced::futures::{self, SinkExt};
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::keyboard::{Event as KeyEvent, Key, Modifiers};
use cosmic::iced::mouse::ScrollDelta;
use cosmic::iced::widget::scrollable;
use cosmic::iced::{Alignment, ContentFit, Length, Rectangle, Subscription, stream, window};
use cosmic::widget::nav_bar::Model;
use cosmic::widget::segmented_button::Entity;
use cosmic::widget::{self};
use cosmic::{Application, Element, action, cosmic_theme, executor, theme};
use rayon::prelude::*;
use std::any::TypeId;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, hash, process};

use crate::fl;

const THUMBNAIL_WIDTH: u16 = 128;

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}

mod argparse;
mod thumbnail;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = argparse::parse();

    if let Some(output) = args.thumbnail_opt {
        let Some(input) = args.url_opt else {
            log::error!("thumbnailer can only handle exactly one URL");
            process::exit(1);
        };

        match thumbnail::main(&input, &output, args.size_opt) {
            Ok(()) => process::exit(0),
            Err(err) => {
                log::error!("failed to thumbnail '{}': {}", input, err);
                process::exit(1);
            }
        }
    }

    #[cfg(all(unix, not(target_os = "redox")))]
    match fork::daemon(true, true) {
        Ok(fork::Fork::Child) => (),
        Ok(fork::Fork::Parent(_child_pid)) => process::exit(0),
        Err(err) => {
            eprintln!("failed to daemonize: {:?}", err);
            process::exit(1);
        }
    }

    crate::localize::localize();

    cosmic::app::run::<App>(
        Settings::default(),
        Flags {
            url_opt: args.url_opt,
        },
    )?;
    Ok(())
}

//TODO: return errors
fn display_list_to_image(display_list: &mupdf::DisplayList, scale: f32) -> widget::image::Handle {
    let matrix = mupdf::Matrix::new_scale(scale, scale);
    let pixmap = display_list
        .to_pixmap(&matrix, &mupdf::Colorspace::device_rgb(), false)
        .unwrap();
    let mut data = Vec::new();
    //TODO: store raw image data?
    pixmap.write_to(&mut data, mupdf::ImageFormat::PNG).unwrap();
    widget::image::Handle::from_bytes(data)
}

struct Flags {
    url_opt: Option<url::Url>,
}

#[derive(Clone, Debug)]
struct Page {
    index: i32,
    bounds: mupdf::Rect,
    display_list: Option<Arc<mupdf::DisplayList>>,
    icon_bounds: Cell<Option<Rectangle>>,
    icon_handle: Option<widget::image::Handle>,
    svg_handle: Option<widget::svg::Handle>,
}

#[derive(Clone, Debug)]
enum Message {
    DisplayList(i32, Arc<mupdf::DisplayList>),
    DocumentMeta(DocumentMeta),
    FileLoad(url::Url),
    FileOpen,
    Fullscreen,
    Key(Modifiers, Key, Option<SmolStr>),
    ModifiersChanged(Modifiers),
    NavScroll(scrollable::Viewport),
    NavSelect(Entity),
    Pages(Vec<Page>),
    PdfBackgroundChange(usize),
    PropertiesToggle,
    SearchActivate,
    SearchClear,
    SearchInput(String),
    SearchResults(Entity, Vec<mupdf::Quad>),
    Svg(Entity, widget::svg::Handle),
    Thumbnail(Entity, widget::image::Handle),
    ZoomDropdown(usize),
    ZoomScroll(ScrollDelta),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PdfBackground {
    SystemTheme,
    Dark,
    Light,
    White,
    Custom { r: u8, g: u8, b: u8 },
}

impl PdfBackground {
    fn to_color(self, is_dark: bool) -> cosmic::iced::Color {
        match self {
            PdfBackground::SystemTheme => {
                if is_dark {
                    cosmic::iced::Color::from_rgb(0.1, 0.1, 0.1)
                } else {
                    cosmic::iced::Color::from_rgb(1.0, 1.0, 1.0)
                }
            }
            PdfBackground::Dark => cosmic::iced::Color::from_rgb(0.1, 0.1, 0.1),
            PdfBackground::Light => cosmic::iced::Color::from_rgb(1.0, 1.0, 1.0),
            PdfBackground::White => cosmic::iced::Color::from_rgb(1.0, 1.0, 1.0),
            PdfBackground::Custom { r, g, b } => {
                cosmic::iced::Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
            }
        }
    }

    fn all() -> &'static [Self] {
        &[
            PdfBackground::SystemTheme,
            PdfBackground::Dark,
            PdfBackground::Light,
            PdfBackground::White,
        ]
    }

    fn resolve(&self, is_dark: bool) -> Self {
        match self {
            PdfBackground::SystemTheme => {
                if is_dark {
                    PdfBackground::Dark
                } else {
                    PdfBackground::Light
                }
            }
            other => *other,
        }
    }
}

impl fmt::Display for PdfBackground {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PdfBackground::SystemTheme => write!(f, "System"),
            PdfBackground::Dark => write!(f, "Dark"),
            PdfBackground::Light => write!(f, "Light"),
            PdfBackground::White => write!(f, "White"),
            PdfBackground::Custom { .. } => write!(f, "Custom"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Zoom {
    FitBoth,
    FitHeight,
    FitWidth,
    Percent(i16),
}

impl Zoom {
    fn all() -> &'static [Self] {
        &[
            Zoom::FitBoth,
            Zoom::FitHeight,
            Zoom::FitWidth,
            Zoom::Percent(25),
            Zoom::Percent(50),
            Zoom::Percent(75),
            Zoom::Percent(100),
            Zoom::Percent(125),
            Zoom::Percent(150),
            Zoom::Percent(175),
            Zoom::Percent(200),
            Zoom::Percent(225),
            Zoom::Percent(250),
            Zoom::Percent(275),
            Zoom::Percent(300),
            Zoom::Percent(325),
            Zoom::Percent(350),
            Zoom::Percent(375),
            Zoom::Percent(400),
            Zoom::Percent(425),
            Zoom::Percent(450),
            Zoom::Percent(475),
            Zoom::Percent(500),
        ]
    }
}

impl fmt::Display for Zoom {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        //TODO: translate?
        match self {
            Zoom::FitBoth => write!(f, "Fit width and height"),
            Zoom::FitHeight => write!(f, "Fit height"),
            Zoom::FitWidth => write!(f, "Fit width"),
            Zoom::Percent(percent) => write!(f, "{}%", percent),
        }
    }
}

#[derive(Clone, Debug)]
struct DocumentMeta {
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    creator: Option<String>,
    producer: Option<String>,
    page_count: i32,
    file_path: PathBuf,
    file_size: u64,
}

struct App {
    core: Core,
    flags: Flags,
    fullscreen: bool,
    modifiers: Modifiers,
    nav_model: Model,
    nav_scroll_id: widget::Id,
    nav_viewport: Option<scrollable::Viewport>,
    pdf_background: PdfBackground,
    pdf_background_names: Vec<String>,
    properties_open: bool,
    document_meta: Option<DocumentMeta>,
    search_active: bool,
    search_id: widget::Id,
    search_term: String,
    view_ratio: Cell<f32>,
    zoom: Zoom,
    zoom_names: Vec<String>,
    zoom_scroll: f32,
}

impl App {
    fn entity_by_index(&self, index: i32) -> Option<Entity> {
        for entity in self.nav_model.iter() {
            if let Some(page) = self.nav_model.data::<Page>(entity)
                && page.index == index
            {
                return Some(entity);
            }
        }
        None
    }

    fn properties_view(&self) -> Element<'_, Message> {
        let spacing = theme::spacing();

        let mut rows = widget::column::with_capacity(8)
            .spacing(spacing.space_m)
            .padding(spacing.space_l);

        if let Some(meta) = &self.document_meta {
            // File info section
            rows = rows.push(
                widget::column::with_capacity(2)
                    .spacing(spacing.space_xxs)
                    .push(widget::text::title4("File"))
                    .push(
                        widget::column::with_capacity(2)
                            .spacing(spacing.space_xxs)
                            .push(self.prop_row("Path", &meta.file_path.to_string_lossy()))
                            .push(self.prop_row(
                                "Size",
                                &format_size(meta.file_size),
                            )),
                    ),
            );

            // Document info section
            let mut doc_info = widget::column::with_capacity(6).spacing(spacing.space_xxs);
            if let Some(title) = &meta.title {
                doc_info = doc_info.push(self.prop_row("Title", title));
            }
            if let Some(author) = &meta.author {
                doc_info = doc_info.push(self.prop_row("Author", author));
            }
            if let Some(subject) = &meta.subject {
                doc_info = doc_info.push(self.prop_row("Subject", subject));
            }
            if let Some(creator) = &meta.creator {
                doc_info = doc_info.push(self.prop_row("Creator", creator));
            }
            if let Some(producer) = &meta.producer {
                doc_info = doc_info.push(self.prop_row("Producer", producer));
            }
            doc_info = doc_info.push(self.prop_row("Pages", &meta.page_count.to_string()));

            rows = rows.push(
                widget::column::with_capacity(2)
                    .spacing(spacing.space_xxs)
                    .push(widget::text::title4("Document"))
                    .push(doc_info),
            );
        } else {
            rows = rows.push(widget::text::body("No document loaded."));
        }

        // Close button
        rows = rows.push(
            widget::container(
                widget::button::standard("Close").on_press(Message::PropertiesToggle),
            )
            .align_x(Alignment::End),
        );

        widget::container(rows)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(spacing.space_l)
            .into()
    }

    fn prop_row(&self, label: &str, value: &str) -> Element<'_, Message> {
        let spacing = theme::spacing();
        widget::row::with_capacity(2)
            .spacing(spacing.space_s)
            .push(widget::text::caption(format!("{}:", label)))
            .push(widget::text::body(value.to_owned()))
            .into()
    }

    fn update_page(&mut self) -> Task<Message> {
        let entity = self.nav_model.active();
        let Some(page) = self.nav_model.data::<Page>(entity) else {
            return Task::none();
        };
        let mut tasks = Vec::with_capacity(2);
        if let Some(viewport) = &self.nav_viewport {
            let mut bounds = viewport.bounds();
            // Adjust bounds to match scroll offset
            let offset = viewport.absolute_offset();
            bounds.x = offset.x;
            bounds.y = offset.y;
            if let Some(icon_bounds) = page.icon_bounds.get() {
                if bounds.y > icon_bounds.y {
                    // Scroll up if necessary
                    tasks.push(scrollable::scroll_to(
                        self.nav_scroll_id.clone(),
                        scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(icon_bounds.y),
                        },
                    ));
                } else if bounds.y + bounds.height < icon_bounds.y + icon_bounds.height {
                    // Scroll down if necessary
                    tasks.push(scrollable::scroll_to(
                        self.nav_scroll_id.clone(),
                        scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(icon_bounds.y + icon_bounds.height - bounds.height),
                        },
                    ));
                }
            }
        }
        if page.svg_handle.is_none()
            && let Some(display_list) = page.display_list.clone()
        {
            tasks.push(Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let svg = display_list.to_svg(&mupdf::Matrix::IDENTITY).unwrap();
                        Message::Svg(entity, widget::svg::Handle::from_memory(svg.into_bytes()))
                    })
                    .await
                    .unwrap()
                },
                action::app,
            ));
        }
        Task::batch(tasks)
    }
}

impl Application for App {
    type Executor = executor::multi::Executor;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = "com.system76.CosmicReader";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        let cosmic_theme::Spacing { space_xxs, .. } = theme::spacing();

        let mut elements = Vec::with_capacity(1);

        if self.search_active {
            elements.push(
                widget::text_input::search_input("", &self.search_term)
                    .width(Length::Fixed(240.0))
                    .id(self.search_id.clone())
                    .on_clear(Message::SearchClear)
                    .on_input(Message::SearchInput)
                    .into(),
            );
        } else {
            elements.push(
                widget::button::icon(widget::icon::from_name("system-search-symbolic"))
                    .on_press(Message::SearchActivate)
                    .padding(space_xxs)
                    .into(),
            );
        }

        elements
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        vec![
            widget::button::icon(widget::icon::from_name("document-properties-symbolic"))
                .on_press(Message::PropertiesToggle)
                .padding(theme::spacing().space_xxs)
                .into(),
            widget::dropdown(
                &self.pdf_background_names,
                PdfBackground::all().iter().position(|bg| bg == &self.pdf_background),
                Message::PdfBackgroundChange,
            )
            .into(),
            widget::dropdown(
                &self.zoom_names,
                Zoom::all().iter().position(|zoom| zoom == &self.zoom),
                Message::ZoomDropdown,
            )
            .into(),
        ]
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Message>) {
        let mut zoom_names = Vec::new();
        for zoom in Zoom::all() {
            zoom_names.push(zoom.to_string());
        }

        let mut pdf_background_names = Vec::new();
        for bg in PdfBackground::all() {
            pdf_background_names.push(bg.to_string());
        }

        let core = core;

        let mut app = Self {
            core,
            flags,
            fullscreen: false,
            modifiers: Modifiers::default(),
            nav_model: Model::default(),
            nav_scroll_id: widget::Id::unique(),
            nav_viewport: None,
            pdf_background: PdfBackground::SystemTheme,
            pdf_background_names,
            properties_open: false,
            document_meta: None,
            search_active: false,
            search_id: widget::Id::unique(),
            search_term: String::new(),
            view_ratio: Cell::new(1.0),
            zoom: Zoom::FitBoth,
            zoom_names,
            zoom_scroll: 0.0,
        };
        let task = app.update_page();
        (app, task)
    }

    fn system_theme_mode_update(
        &mut self,
        _keys: &[&'static str],
        _new_theme: &cosmic_theme::ThemeMode,
    ) -> Task<Message> {
        if self.pdf_background == PdfBackground::SystemTheme {
            return self.update_page();
        }
        Task::none()
    }

    fn nav_bar(&self) -> Option<Element<'_, action::Action<Message>>> {
        if !self.core.nav_bar_active() || self.fullscreen {
            return None;
        }

        let cosmic_theme::Spacing { space_xxs, .. } = theme::spacing();

        let mut column = widget::column::with_capacity(self.nav_model.len())
            .padding(space_xxs)
            .spacing(space_xxs);
        let x = space_xxs as f32;
        let mut y = space_xxs as f32;
        let mut count = 0;
        for entity in self.nav_model.iter() {
            if let Some(page) = self.nav_model.data::<Page>(entity) {
                if count > 0 {
                    y += space_xxs as f32;
                }
                //TODO: cache sizes during icon generation?
                let width = THUMBNAIL_WIDTH as f32;
                let height = page.bounds.height() * width / page.bounds.width();
                page.icon_bounds.set(Some(Rectangle {
                    x,
                    y,
                    width,
                    height,
                }));
                if let Some(handle) = &page.icon_handle {
                    column = column.push(
                        widget::button::image(handle)
                            .width(width)
                            .height(height)
                            .on_press(action::app(Message::NavSelect(entity)))
                            .selected(entity == self.nav_model.active()),
                    );
                } else {
                    column = column.push(
                        widget::button::custom_image_button(
                            widget::space::vertical().height(height),
                            None,
                        )
                        .width(width)
                        .height(height)
                        .on_press(action::app(Message::NavSelect(entity)))
                        .selected(entity == self.nav_model.active()),
                    );
                }
                y += height;
                count += 1;
            }
        }

        let mut nav = widget::container(
            scrollable(column)
                .id(self.nav_scroll_id.clone())
                .on_scroll(|x| action::app(Message::NavScroll(x)))
                .width(Length::Fixed(
                    (THUMBNAIL_WIDTH as f32) + (space_xxs as f32) * 2.0,
                )),
        );
        if !self.core.is_condensed() {
            nav = nav.max_width(280);
        }
        Some(nav.into())
    }

    fn nav_model(&self) -> Option<&Model> {
        Some(&self.nav_model)
    }

    fn on_nav_select(&mut self, id: widget::nav_bar::Id) -> Task<Message> {
        self.nav_model.activate(id);
        self.update_page()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::DisplayList(index, display_list) => {
                if let Some(entity) = self.entity_by_index(index) {
                    let mut tasks = Vec::with_capacity(2);
                    if let Some(page) = self.nav_model.data_mut::<Page>(entity) {
                        page.display_list = Some(display_list.clone());
                    }
                    if entity == self.nav_model.active() {
                        tasks.push(self.update_page());
                    }
                    tasks.push(Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                let scale =
                                    (THUMBNAIL_WIDTH as f32) / display_list.bounds().width();
                                Message::Thumbnail(
                                    entity,
                                    display_list_to_image(&display_list, scale),
                                )
                            })
                            .await
                            .unwrap()
                        },
                        action::app,
                    ));
                    return Task::batch(tasks);
                }
            }
            Message::DocumentMeta(meta) => {
                self.document_meta = Some(meta);
            }
            Message::FileLoad(url) => {
                self.nav_model.clear();
                self.flags.url_opt = Some(url);
                self.document_meta = None;
                self.properties_open = false;
            }
            Message::FileOpen => {
                #[cfg(feature = "xdg-portal")]
                return Task::perform(
                    async move {
                        let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                            .title(fl!("open-file"));
                        match dialog.open_file().await {
                            Ok(response) => {
                                action::app(Message::FileLoad(response.url().to_owned()))
                            }
                            Err(err) => {
                                log::warn!("failed to open file: {}", err);
                                action::none()
                            }
                        }
                    },
                    |x| x,
                );
            }
            Message::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                self.core.window.show_headerbar = !self.fullscreen;
                if let Some(window_id) = self.core.main_window_id() {
                    return window::set_mode(
                        window_id,
                        if self.fullscreen {
                            window::Mode::Fullscreen
                        } else {
                            window::Mode::Windowed
                        },
                    );
                }
            }
            //TODO: move to key binds and set up menu
            Message::Key(_modifiers, key, _text) => match &key {
                Key::Named(Named::ArrowUp | Named::ArrowLeft | Named::PageUp) => {
                    let pos = self
                        .nav_model
                        .position(self.nav_model.active())
                        .unwrap_or(0);
                    if let Some(new_pos) = pos.checked_sub(1) {
                        self.nav_model.activate_position(new_pos);
                    }
                    return self.update_page();
                }
                Key::Named(Named::ArrowDown | Named::ArrowRight | Named::PageDown) => {
                    let pos = self
                        .nav_model
                        .position(self.nav_model.active())
                        .unwrap_or(0);
                    if let Some(new_pos) = pos.checked_add(1) {
                        self.nav_model.activate_position(new_pos);
                    }
                    return self.update_page();
                }
                Key::Named(Named::Enter) => {
                    return self.update(Message::Fullscreen);
                }
                Key::Named(Named::Escape) => {
                    self.search_active = false;
                }
                Key::Character(c) => match c.as_str() {
                    "0" => {
                        self.zoom = Zoom::Percent(100);
                    }
                    "-" => {
                        let percent = match self.zoom {
                            Zoom::Percent(percent) => percent,
                            _ => ((self.view_ratio.get() * 4.0).round() as i16) * 25,
                        };
                        self.zoom = Zoom::Percent((percent - 25).clamp(25, 500));
                    }
                    "=" => {
                        let percent = match self.zoom {
                            Zoom::Percent(percent) => percent,
                            _ => ((self.view_ratio.get() * 4.0).round() as i16) * 25,
                        };
                        self.zoom = Zoom::Percent((percent + 25).clamp(25, 500));
                    }
                    "f" => {
                        self.zoom = Zoom::FitBoth;
                    }
                    "h" => {
                        self.zoom = Zoom::FitHeight;
                    }
                    "w" => {
                        self.zoom = Zoom::FitWidth;
                    }
                    "s" | "/" => {
                        self.search_active = true;
                        return widget::text_input::focus(self.search_id.clone());
                    }
                    _ => {}
                },
                _ => {}
            },
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
            }
            Message::NavScroll(viewport) => {
                self.nav_viewport = Some(viewport);
            }
            Message::NavSelect(entity) => {
                return self.on_nav_select(entity);
            }
            Message::Pages(pages) => {
                self.nav_model.clear();
                for page in pages {
                    self.nav_model.insert().data::<Page>(page);
                }
                self.nav_model.activate_position(0);
                return self.update_page();
            }
            Message::PropertiesToggle => {
                self.properties_open = !self.properties_open;
            }
            Message::SearchActivate => {
                self.search_active = true;
                return widget::text_input::focus(self.search_id.clone());
            }
            Message::SearchClear => {
                self.search_active = false;
            }
            Message::SearchInput(term) => {
                self.search_term = term.clone();
            }
            Message::SearchResults(_entity, _quads) => {
                //TODO
            }
            Message::Svg(entity, handle) => {
                if let Some(page) = self.nav_model.data_mut::<Page>(entity) {
                    page.svg_handle = Some(handle);
                }
            }
            Message::Thumbnail(entity, handle) => {
                if let Some(page) = self.nav_model.data_mut::<Page>(entity) {
                    page.icon_handle = Some(handle);
                }
            }
            Message::PdfBackgroundChange(index) => {
                if let Some(bg) = PdfBackground::all().get(index) {
                    self.pdf_background = *bg;
                }
            }
            Message::ZoomDropdown(index) => {
                if let Some(zoom) = Zoom::all().get(index) {
                    self.zoom = *zoom;
                }
            }
            Message::ZoomScroll(delta) => {
                self.zoom_scroll += match delta {
                    ScrollDelta::Lines { y, .. } => y,
                    //TODO: best pixel to line conversion ratio?
                    ScrollDelta::Pixels { y, .. } => y / 20.0,
                };
                let mut percent = match self.zoom {
                    Zoom::Percent(percent) => percent,
                    _ => ((self.view_ratio.get() * 4.0).round() as i16) * 25,
                };
                while self.zoom_scroll >= 1.0 {
                    percent += 25;
                    self.zoom_scroll -= 1.0;
                }
                while self.zoom_scroll <= -1.0 {
                    percent -= 25;
                    self.zoom_scroll += 1.0;
                }
                self.zoom = Zoom::Percent(percent.clamp(25, 500));
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let entity = self.nav_model.active();

        if self.properties_open {
            return self.properties_view();
        }

        // Handle cached images
        if let Some(page) = self.nav_model.data::<Page>(entity) {
            let pdf_background = self.pdf_background;
            let zoom = self.zoom;
            let page_bounds = page.bounds;
            let svg_handle = page.svg_handle.clone();
            let view_ratio = self.view_ratio.clone();
            let is_dark = theme::is_dark();
            
            return widget::responsive(move |size| {
                let ratio = match zoom {
                    Zoom::FitHeight => size.height / page_bounds.height(),
                    Zoom::FitWidth => size.width / page_bounds.width(),
                    Zoom::FitBoth => {
                        (size.width / page_bounds.width()).min(size.height / page_bounds.height())
                    }
                    //TODO: adjust ratio by DPI
                    Zoom::Percent(percent) => (percent as f32) / 100.0,
                };
                view_ratio.set(ratio);
                let width = page_bounds.width() * ratio;
                let height = page_bounds.height() * ratio;
                let bg_color = pdf_background.to_color(is_dark);
                let mut container = widget::container(
                    widget::container(if let Some(handle) = &svg_handle {
                        Element::from(
                            widget::svg(handle.clone())
                                .content_fit(ContentFit::Fill)
                                .width(width)
                                .height(height),
                        )
                    } else {
                        Element::from(widget::space().width(width).height(height))
                    })
                    .style(move |_theme| widget::container::background(bg_color)),
                );
                if size.width > width {
                    container = container.center_x(size.width);
                }
                if size.height > height {
                    container = container.center_y(size.height);
                }
                let mut mouse_area =
                    widget::mouse_area(container).on_double_press(Message::Fullscreen);
                if self.modifiers.contains(Modifiers::CTRL) {
                    mouse_area = mouse_area.on_scroll(Message::ZoomScroll);
                }
                scrollable(mouse_area)
                    .direction(scrollable::Direction::Both {
                        vertical: Default::default(),
                        horizontal: Default::default(),
                    })
                    .into()
            })
            .into();
        }

        if self.flags.url_opt.is_none() {
            let column = widget::column::with_capacity(4)
                .align_x(Alignment::Center)
                .spacing(24)
                .width(Length::Fill)
                .height(Length::Fill)
                .push(widget::space::vertical())
                .push(
                    widget::column::with_capacity(2)
                        .align_x(Alignment::Center)
                        .spacing(8)
                        .push(widget::icon::from_name("folder-symbolic").size(64))
                        .push(widget::text::body(fl!("no-file-open"))),
                )
                .push(widget::button::suggested(fl!("open-file")).on_press(Message::FileOpen))
                .push(widget::space::vertical());

            return column.into();
        }

        widget::space::horizontal().into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::with_capacity(3);

        subscriptions.push(event::listen_with(
            |event, status, _window_id| match event {
                Event::Keyboard(KeyEvent::KeyPressed {
                    key,
                    modifiers,
                    text,
                    ..
                }) => match status {
                    event::Status::Ignored => Some(Message::Key(modifiers, key, text)),
                    event::Status::Captured => None,
                },
                Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => {
                    Some(Message::ModifiersChanged(modifiers))
                }
                _ => None,
            },
        ));

        struct LoaderSubscription;
        if let Some(url) = self.flags.url_opt.clone() {
            subscriptions.push(Subscription::run_with(
                (TypeId::of::<LoaderSubscription>(), url),
                |(_, url)| {
                    let url = url.clone();
                    stream::channel(
                        16,
                        |mut output: futures::channel::mpsc::Sender<Message>| async move {
                            //TODO: send errors to UI
                            let handle = tokio::runtime::Handle::current();
                            tokio::task::spawn_blocking(move || {
                                let Ok(path) = url.to_file_path() else { return };
                                let doc = mupdf::Document::open(path.as_os_str()).unwrap();
                                let page_count = doc.page_count().unwrap();

                                let file_size = std::fs::metadata(&path)
                                    .map(|m| m.len())
                                    .unwrap_or(0);

                                let meta = DocumentMeta {
                                    title: doc.metadata(mupdf::MetadataName::Title).ok().and_then(|t| {
                                        if t.is_empty() { None } else { Some(t) }
                                    }),
                                    author: doc.metadata(mupdf::MetadataName::Author).ok().and_then(|t| {
                                        if t.is_empty() { None } else { Some(t) }
                                    }),
                                    subject: doc.metadata(mupdf::MetadataName::Subject).ok().and_then(|t| {
                                        if t.is_empty() { None } else { Some(t) }
                                    }),
                                    creator: doc.metadata(mupdf::MetadataName::Creator).ok().and_then(|t| {
                                        if t.is_empty() { None } else { Some(t) }
                                    }),
                                    producer: doc.metadata(mupdf::MetadataName::Producer).ok().and_then(|t| {
                                        if t.is_empty() { None } else { Some(t) }
                                    }),
                                    page_count,
                                    file_path: path.clone(),
                                    file_size,
                                };
                                handle
                                    .block_on(async {
                                        output.send(Message::DocumentMeta(meta)).await
                                    })
                                    .unwrap();

                                // Generate the table of contents
                                let mut pages =
                                    Vec::with_capacity(usize::try_from(page_count).unwrap());
                                for index in 0..page_count {
                                    let page = doc.load_page(index).unwrap();
                                    //TODO: get label?
                                    let bounds = page.bounds().unwrap();
                                    pages.push(Page {
                                        index,
                                        bounds,
                                        display_list: None,
                                        icon_bounds: Cell::new(None),
                                        icon_handle: None,
                                        svg_handle: None,
                                    });
                                }
                                handle
                                    .block_on(async { output.send(Message::Pages(pages)).await })
                                    .unwrap();

                                // Generate display lists (cannot be threaded)
                                for index in 0..page_count {
                                    let page = doc.load_page(index).unwrap();
                                    let display_list = page.to_display_list(false).unwrap();
                                    handle
                                        .block_on(async {
                                            output
                                                .send(Message::DisplayList(
                                                    index,
                                                    Arc::new(display_list),
                                                ))
                                                .await
                                        })
                                        .unwrap();
                                }
                            })
                            .await
                            .unwrap();
                            std::future::pending().await
                        },
                    )
                },
            ));
        }

        if self.search_active && !self.search_term.is_empty() {
            //TODO: efficiently cache this somehow
            let mut display_lists = Vec::with_capacity(self.nav_model.len());
            for entity in self.nav_model.iter() {
                if let Some(page) = self.nav_model.data::<Page>(entity)
                    && let Some(display_list) = page.display_list.clone()
                {
                    display_lists.push((entity, display_list));
                }
            }

            struct SearchSubscription;
            struct Wrapper {
                term: String,
                display_lists: Vec<(Entity, Arc<mupdf::DisplayList>)>,
            }
            impl hash::Hash for Wrapper {
                fn hash<H: hash::Hasher>(&self, state: &mut H) {
                    TypeId::of::<SearchSubscription>().hash(state);
                    self.term.hash(state);
                }
            }
            subscriptions.push(Subscription::run_with(
                Wrapper {
                    term: self.search_term.clone(),
                    display_lists,
                },
                |Wrapper {
                     term,
                     display_lists,
                 }| {
                    let term = term.clone();
                    let display_lists = display_lists.clone();
                    stream::channel(
                        16,
                        |output: futures::channel::mpsc::Sender<Message>| async move {
                            let output = Arc::new(tokio::sync::Mutex::new(output));
                            let handle = tokio::runtime::Handle::current();
                            tokio::task::spawn_blocking(move || {
                                let _timer = std::time::Instant::now();
                                display_lists.par_iter().for_each(|(entity, display_list)| {
                                    let quads = display_list.search(&term, 100).unwrap();
                                    if !quads.is_empty() {
                                        let quads_vec: Vec<mupdf::Quad> =
                                            quads.into_iter().collect();
                                        let output = output.clone();
                                        handle
                                            .block_on(async move {
                                                output
                                                    .lock()
                                                    .await
                                                    .send(Message::SearchResults(
                                                        *entity, quads_vec,
                                                    ))
                                                    .await
                                            })
                                            .unwrap();
                                    }
                                })
                            })
                            .await
                            .unwrap();
                            std::future::pending().await
                        },
                    )
                },
            ));
        }

        Subscription::batch(subscriptions)
    }
}
