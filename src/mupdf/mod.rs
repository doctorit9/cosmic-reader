use cosmic::app::{Core, Settings, Task};
use cosmic::iced::core::SmolStr;
use cosmic::iced::event::{self, Event};
use cosmic::iced::futures::{self, SinkExt};
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::keyboard::{Event as KeyEvent, Key, Modifiers};
use cosmic::iced::mouse::{self, Button, ScrollDelta};
use cosmic::iced::widget::scrollable;
use cosmic::iced::{Alignment, Length, Rectangle, Subscription, stream, window};
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

fn display_list_to_raw(display_list: &mupdf::DisplayList, scale: f32) -> (widget::image::Handle, widget::image::Handle) {
    let matrix = mupdf::Matrix::new_scale(scale, scale);
    let mut pixmap = display_list
        .to_pixmap(&matrix, &mupdf::Colorspace::device_rgb(), true)
        .unwrap();
    let w = pixmap.width();
    let h = pixmap.height();
    let normal = widget::image::Handle::from_rgba(w, h, pixmap.samples().to_vec());
    {
        let samples = pixmap.samples_mut();
        for chunk in samples.chunks_exact_mut(4) {
            chunk[0] = 255 - chunk[0];
            chunk[1] = 255 - chunk[1];
            chunk[2] = 255 - chunk[2];
        }
    }
    let inverted = widget::image::Handle::from_rgba(w, h, pixmap.samples().to_vec());
    (normal, inverted)
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
    image_handle: Option<widget::image::Handle>,
    inverted_image_handle: Option<widget::image::Handle>,
}

#[derive(Clone, Debug)]
enum Message {
    ContentScroll(scrollable::Viewport),
    DisplayList(i32, Arc<mupdf::DisplayList>),
    DocumentMeta(DocumentMeta),
    FileLoad(url::Url),
    FileOpen,
    Fullscreen,
    Key(Modifiers, Key, Option<SmolStr>),
    LoadError(String),
    MiddleDragRelease,
    MiddleDragStart(cosmic::iced::Point),
    MiddleDragMove(cosmic::iced::Point),
    ModifiersChanged(Modifiers),
    NavScroll(scrollable::Viewport),
    NavSelect(Entity),
    NaturalScrollToggle,
    PageRendered(Entity, widget::image::Handle, widget::image::Handle),
    PageScroll(ScrollDelta),
    Pages(Vec<Page>),
    PdfBackgroundChange(usize),
    PropertiesToggle,
    SearchActivate,
    SearchClear,
    SearchInput(String),
    SearchResults(Entity, Vec<mupdf::Quad>),
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
        }
    }

    /// Whether this background mode inverts PDF content colors for dark reading.
    fn inverts_content(self, is_dark: bool) -> bool {
        match self {
            PdfBackground::SystemTheme => is_dark,
            PdfBackground::Dark => true,
            _ => false,
        }
    }

    fn all() -> &'static [Self] {
        &[
            PdfBackground::SystemTheme,
            PdfBackground::White,
            PdfBackground::Light,
            PdfBackground::Dark,
        ]
    }

    }

impl fmt::Display for PdfBackground {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PdfBackground::SystemTheme => write!(f, "System"),
            PdfBackground::Dark => write!(f, "Dark"),
            PdfBackground::Light => write!(f, "Light"),
            PdfBackground::White => write!(f, "White"),
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
    content_scroll_id: widget::Id,
    content_viewport: Option<scrollable::Viewport>,
    core: Core,
    flags: Flags,
    fullscreen: bool,
    load_error: Option<String>,
    middle_drag_pos: Option<cosmic::iced::Point>,
    modifiers: Modifiers,
    natural_scroll: bool,
    nav_model: Model,
    nav_scroll_id: widget::Id,
    nav_viewport: Option<scrollable::Viewport>,
    page_scroll: f32,
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
        let mut sections = Vec::new();

        if let Some(meta) = &self.document_meta {
            sections.push(
                widget::settings::section()
                    .title("File")
                    .add(widget::settings::item(
                        "Path",
                        widget::text::body(meta.file_path.to_string_lossy().to_string()),
                    ))
                    .add(widget::settings::item(
                        "Size",
                        widget::text::body(format_size(meta.file_size)),
                    ))
                    .into(),
            );

            let mut doc_section = widget::settings::section().title("Document");
            doc_section = doc_section.add(widget::settings::item(
                "Pages",
                widget::text::body(meta.page_count.to_string()),
            ));
            if let Some(title) = &meta.title {
                doc_section = doc_section.add(widget::settings::item(
                    "Title",
                    widget::text::body(title.clone()),
                ));
            }
            if let Some(author) = &meta.author {
                doc_section = doc_section.add(widget::settings::item(
                    "Author",
                    widget::text::body(author.clone()),
                ));
            }
            if let Some(subject) = &meta.subject {
                doc_section = doc_section.add(widget::settings::item(
                    "Subject",
                    widget::text::body(subject.clone()),
                ));
            }
            if let Some(creator) = &meta.creator {
                doc_section = doc_section.add(widget::settings::item(
                    "Creator",
                    widget::text::body(creator.clone()),
                ));
            }
            if let Some(producer) = &meta.producer {
                doc_section = doc_section.add(widget::settings::item(
                    "Producer",
                    widget::text::body(producer.clone()),
                ));
            }
            sections.push(doc_section.into());
        }

        sections.push(
            widget::settings::section()
                .title("Settings")
                .add(widget::settings::item(
                    "Natural scrolling",
                    widget::toggler(self.natural_scroll)
                        .on_toggle(|_| Message::NaturalScrollToggle),
                ))
                .into(),
        );

        widget::settings::view_column(sections)
            .width(Length::Fill)
            .height(Length::Fill)
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
        if page.image_handle.is_none()
            && let Some(display_list) = page.display_list.clone()
        {
            tasks.push(Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        display_list_to_raw(&display_list, 3.0)
                    })
                    .await
                    .unwrap()
                },
                move |(normal, inverted)| {
                    action::app(Message::PageRendered(entity, normal, inverted))
                },
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
            content_scroll_id: widget::Id::unique(),
            content_viewport: None,
            core,
            flags,
            fullscreen: false,
            load_error: None,
            middle_drag_pos: None,
            modifiers: Modifiers::default(),
            natural_scroll: true,
            nav_model: Model::default(),
            nav_scroll_id: widget::Id::unique(),
            nav_viewport: None,
            page_scroll: 0.0,
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
            Message::ContentScroll(viewport) => {
                self.content_viewport = Some(viewport);
            }
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
                                let scale = (THUMBNAIL_WIDTH as f32) / display_list.bounds().width();
                                let matrix = mupdf::Matrix::new_scale(scale, scale);
                                let pixmap = display_list
                                    .to_pixmap(&matrix, &mupdf::Colorspace::device_rgb(), true)
                                    .unwrap();
                                let w = pixmap.width();
                                let h = pixmap.height();
                                let handle = widget::image::Handle::from_rgba(w, h, pixmap.samples().to_vec());
                                Message::Thumbnail(entity, handle)
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
                self.load_error = None;
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
            Message::MiddleDragStart(pos) => {
                self.middle_drag_pos = Some(pos);
            }
            Message::MiddleDragMove(pos) => {
                if let Some(last_pos) = self.middle_drag_pos.take() {
                    let offset = self.content_viewport.as_ref().map(|v| v.absolute_offset()).unwrap_or_default();
                    let new_x = (offset.x + last_pos.x - pos.x).max(0.0);
                    let new_y = (offset.y + last_pos.y - pos.y).max(0.0);
                    self.middle_drag_pos = Some(pos);
                    return scrollable::scroll_to(
                        self.content_scroll_id.clone(),
                        scrollable::AbsoluteOffset {
                            x: Some(new_x),
                            y: Some(new_y),
                        },
                    );
                }
            }
            Message::MiddleDragRelease => {
                self.middle_drag_pos = None;
            }
            Message::LoadError(msg) => {
                self.load_error = Some(msg);
                self.document_meta = None;
                self.nav_model.clear();
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
            Message::PageRendered(entity, normal, inverted) => {
                if let Some(page) = self.nav_model.data_mut::<Page>(entity) {
                    page.image_handle = Some(normal);
                    page.inverted_image_handle = Some(inverted);
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
                let scroll_amount = match delta {
                    ScrollDelta::Lines { y, .. } => y,
                    ScrollDelta::Pixels { y, .. } => y / 20.0,
                };
                self.zoom_scroll += scroll_amount;
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
            Message::NaturalScrollToggle => {
                self.natural_scroll = !self.natural_scroll;
            }
            Message::PageScroll(delta) => {
                let scroll_amount = match delta {
                    ScrollDelta::Lines { y, .. } => y / 3.0,
                    ScrollDelta::Pixels { y, .. } => y / 60.0,
                };
                let scroll_amount = if self.natural_scroll {
                    -scroll_amount
                } else {
                    scroll_amount
                };
                self.page_scroll += scroll_amount;
                while self.page_scroll >= 1.0 {
                    self.page_scroll -= 1.0;
                    let pos = self
                        .nav_model
                        .position(self.nav_model.active())
                        .unwrap_or(0);
                    if let Some(new_pos) = pos.checked_add(1) {
                        self.nav_model.activate_position(new_pos);
                        return self.update_page();
                    }
                }
                while self.page_scroll <= -1.0 {
                    self.page_scroll += 1.0;
                    let pos = self
                        .nav_model
                        .position(self.nav_model.active())
                        .unwrap_or(0);
                    if let Some(new_pos) = pos.checked_sub(1) {
                        self.nav_model.activate_position(new_pos);
                        return self.update_page();
                    }
                }
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
            let image_handle = page.image_handle.clone();
            let inverted_image_handle = page.inverted_image_handle.clone();
            let view_ratio = self.view_ratio.clone();
            let is_dark = theme::is_dark();
            let use_inverted = pdf_background.inverts_content(is_dark);
            
            return widget::responsive(move |size| {
                let ratio = match zoom {
                    Zoom::FitHeight => size.height / page_bounds.height(),
                    Zoom::FitWidth => size.width / page_bounds.width(),
                    Zoom::FitBoth => {
                        (size.width / page_bounds.width()).min(size.height / page_bounds.height())
                    }
                    Zoom::Percent(percent) => (percent as f32) / 100.0,
                };
                view_ratio.set(ratio);
                let width = page_bounds.width() * ratio;
                let height = page_bounds.height() * ratio;
                let bg_color = pdf_background.to_color(is_dark);
                let content: Element<'_, Message> = if use_inverted {
                    if let Some(handle) = &inverted_image_handle {
                        Element::from(
                            widget::image(handle.clone())
                                .width(width)
                                .height(height),
                        )
                    } else {
                        Element::from(widget::space().width(width).height(height))
                    }
                } else if let Some(handle) = &image_handle {
                    Element::from(
                        widget::image(handle.clone())
                            .width(width)
                            .height(height),
                    )
                } else {
                    Element::from(widget::space().width(width).height(height))
                };
                let mut container = widget::container(
                    widget::container(content)
                        .style(move |_theme| widget::container::background(bg_color)),
                );
                if size.width > width {
                    container = container.center_x(size.width);
                }
                if size.height > height {
                    container = container.center_y(size.height);
                }
                let mut mouse_area =
                    widget::mouse_area(container)
                        .on_double_press(Message::Fullscreen);
                if self.modifiers.contains(Modifiers::CTRL) {
                    mouse_area = mouse_area.on_scroll(Message::ZoomScroll);
                } else {
                    mouse_area = mouse_area.on_scroll(Message::PageScroll);
                }
                let content = scrollable(mouse_area)
                    .id(self.content_scroll_id.clone())
                    .on_scroll(Message::ContentScroll)
                    .direction(scrollable::Direction::Both {
                        vertical: Default::default(),
                        horizontal: Default::default(),
                    });
                content.into()
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
                Event::Mouse(mouse::Event::ButtonPressed(Button::Middle)) => {
                    Some(Message::MiddleDragStart(Default::default()))
                }
                Event::Mouse(mouse::Event::ButtonReleased(Button::Middle)) => {
                    Some(Message::MiddleDragRelease)
                }
                Event::Mouse(mouse::Event::CursorMoved { position }) => {
                    Some(Message::MiddleDragMove(position))
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
                            let handle = tokio::runtime::Handle::current();
                            let _ = tokio::task::spawn_blocking(move || {
                                let path = match url.to_file_path() {
                                    Ok(p) => p,
                                    Err(e) => {
                                        let _ = handle.block_on(async {
                                            output.send(Message::LoadError(format!("Invalid URL path: {:?}", e))).await
                                        });
                                        return;
                                    }
                                };

                                let doc = match mupdf::Document::open(path.as_os_str()) {
                                    Ok(d) => d,
                                    Err(e) => {
                                        let _ = handle.block_on(async {
                                            output.send(Message::LoadError(format!("Failed to open document: {}", e))).await
                                        });
                                        return;
                                    }
                                };

                                let page_count = match doc.page_count() {
                                    Ok(c) => c,
                                    Err(e) => {
                                        let _ = handle.block_on(async {
                                            output.send(Message::LoadError(format!("Failed to get page count: {}", e))).await
                                        });
                                        return;
                                    }
                                };

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

                                if handle.block_on(async {
                                    output.send(Message::DocumentMeta(meta)).await
                                }).is_err() {
                                    log::warn!("failed to send document meta");
                                    return;
                                }

                                // Generate the table of contents
                                let page_count_usize = match usize::try_from(page_count) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        let _ = handle.block_on(async {
                                            output.send(Message::LoadError(format!("Invalid page count: {}", e))).await
                                        });
                                        return;
                                    }
                                };

                                let mut pages = Vec::with_capacity(page_count_usize);
                                for index in 0..page_count {
                                    let page = match doc.load_page(index) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            let _ = handle.block_on(async {
                                                output.send(Message::LoadError(format!("Failed to load page {}: {}", index, e))).await
                                            });
                                            return;
                                        }
                                    };
                                    let bounds = match page.bounds() {
                                        Ok(b) => b,
                                        Err(e) => {
                                            let _ = handle.block_on(async {
                                                output.send(Message::LoadError(format!("Failed to get bounds for page {}: {}", index, e))).await
                                            });
                                            return;
                                        }
                                    };
                                    pages.push(Page {
                                        index,
                                        bounds,
                                        display_list: None,
                                        icon_bounds: Cell::new(None),
                                        icon_handle: None,
                                        image_handle: None,
                                        inverted_image_handle: None,
                                    });
                                }

                                if handle.block_on(async { output.send(Message::Pages(pages)).await }).is_err() {
                                    log::warn!("failed to send pages");
                                    return;
                                }

                                // Generate display lists (cannot be threaded)
                                for index in 0..page_count {
                                    let page = match doc.load_page(index) {
                                        Ok(p) => p,
                                        Err(e) => {
                                            let _ = handle.block_on(async {
                                                output.send(Message::LoadError(format!("Failed to load page {}: {}", index, e))).await
                                            });
                                            return;
                                        }
                                    };
                                    let display_list = match page.to_display_list(false) {
                                        Ok(dl) => dl,
                                        Err(e) => {
                                            let _ = handle.block_on(async {
                                                output.send(Message::LoadError(format!("Failed to create display list for page {}: {}", index, e))).await
                                            });
                                            return;
                                        }
                                    };
                                    if handle.block_on(async {
                                        output
                                            .send(Message::DisplayList(
                                                index,
                                                Arc::new(display_list),
                                            ))
                                            .await
                                    }).is_err() {
                                        log::warn!("failed to send display list for page {}", index);
                                        return;
                                    }
                                }
                            })
                            .await;
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
