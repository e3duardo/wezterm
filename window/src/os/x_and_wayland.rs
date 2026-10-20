#![cfg(all(unix, not(target_os = "macos")))]

use crate::connection::ConnectionOps;
#[cfg(feature = "wayland")]
use crate::os::wayland::connection::WaylandConnection;
#[cfg(feature = "wayland")]
use crate::os::wayland::window::WaylandWindow;
#[cfg(feature = "gtk")]
use crate::os::gtk4::connection::GtkConnection;
#[cfg(feature = "gtk")]
use crate::os::gtk4::window::GtkWindow;
use crate::os::x11::connection::XConnection;
use crate::os::x11::window::XWindow;
use crate::screen::Screens;
use crate::{
    Appearance, Clipboard, CursorIcon, Rect, RequestedWindowGeometry, ResizeIncrement, ScreenPoint,
    WindowEvent, WindowOps,
};
use async_trait::async_trait;
use config::ConfigHandle;
use promise::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use std::any::Any;
use std::rc::Rc;
use wezterm_font::FontConfiguration;

pub enum Connection {
    X11(Rc<XConnection>),
    #[cfg(feature = "wayland")]
    Wayland(Rc<WaylandConnection>),
    #[cfg(feature = "gtk")]
    Gtk(Rc<GtkConnection>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum Window {
    X11(XWindow),
    #[cfg(feature = "wayland")]
    Wayland(WaylandWindow),
    #[cfg(feature = "gtk")]
    Gtk(GtkWindow),
}

impl Connection {
    pub(crate) fn create_new() -> anyhow::Result<Connection> {
        // Atrás de variável de ambiente só enquanto esse backend está em
        // desenvolvimento (marco 4 do plano) - não interfere em nada do
        // caminho normal (wayland/x11) a menos que explicitamente pedido.
        #[cfg(feature = "gtk")]
        if std::env::var_os("WEZTERM_GTK").is_some() {
            eprintln!("GTK DEBUG: chamando GtkConnection::create_new()");
            match GtkConnection::create_new() {
                Ok(g) => {
                    eprintln!("GTK DEBUG: GtkConnection::create_new() OK, retornando Connection::Gtk");
                    log::debug!("Using gtk connection!");
                    return Ok(Connection::Gtk(Rc::new(g)));
                }
                Err(e) => {
                    eprintln!("GTK DEBUG: GtkConnection::create_new() falhou: {e:#}");
                    log::debug!("Failed to init gtk: {}", e);
                }
            }
        }
        #[cfg(feature = "wayland")]
        if config::configuration().enable_wayland {
            match WaylandConnection::create_new() {
                Ok(w) => {
                    log::debug!("Using wayland connection!");
                    return Ok(Connection::Wayland(Rc::new(w)));
                }
                Err(e) => {
                    log::debug!("Failed to init wayland: {}", e);
                }
            }
        }
        Ok(Connection::X11(XConnection::create_new()?))
    }

    pub async fn new_window<F>(
        &self,
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        match self {
            Self::X11(_) => {
                XWindow::new_window(
                    class_name,
                    name,
                    geometry,
                    config,
                    font_config,
                    event_handler,
                )
                .await
            }
            #[cfg(feature = "wayland")]
            Self::Wayland(_) => {
                WaylandWindow::new_window(
                    class_name,
                    name,
                    geometry,
                    config,
                    font_config,
                    event_handler,
                )
                .await
            }
            #[cfg(feature = "gtk")]
            Self::Gtk(_) => {
                GtkWindow::new_window(
                    class_name,
                    name,
                    geometry,
                    config,
                    font_config,
                    event_handler,
                )
                .await
            }
        }
    }

    pub(crate) fn x11(&self) -> Rc<XConnection> {
        match self {
            Self::X11(x) => Rc::clone(x),
            #[cfg(any(feature = "wayland", feature = "gtk"))]
            _ => panic!("attempted to get x11 reference on non-x11 connection"),
        }
    }

    #[cfg(feature = "wayland")]
    pub(crate) fn wayland(&self) -> Rc<WaylandConnection> {
        match self {
            Self::Wayland(w) => Rc::clone(w),
            _ => panic!("attempted to get wayland reference on non-wayland connection"),
        }
    }

    #[cfg(feature = "gtk")]
    pub(crate) fn gtk(&self) -> Rc<GtkConnection> {
        match self {
            Self::Gtk(g) => Rc::clone(g),
            _ => panic!("attempted to get gtk reference on non-gtk connection"),
        }
    }

    pub(crate) fn advise_of_appearance_change(&self, appearance: Appearance) {
        log::trace!("Appearance changed to {appearance:?}");
        match self {
            Self::X11(x) => x.advise_of_appearance_change(appearance),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.advise_of_appearance_change(appearance),
            #[cfg(feature = "gtk")]
            Self::Gtk(_) => {}
        }
    }
}

impl ConnectionOps for Connection {
    fn name(&self) -> String {
        match self {
            Self::X11(x) => x.name(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.name(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.name(),
        }
    }

    fn terminate_message_loop(&self) {
        match self {
            Self::X11(x) => x.terminate_message_loop(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.terminate_message_loop(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.terminate_message_loop(),
        }
    }

    fn default_dpi(&self) -> f64 {
        match self {
            Self::X11(x) => x.default_dpi(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.default_dpi(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.default_dpi(),
        }
    }

    fn run_message_loop(&self) -> anyhow::Result<()> {
        crate::os::xdg_desktop_portal::subscribe();
        match self {
            Self::X11(x) => x.run_message_loop(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.run_message_loop(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.run_message_loop(),
        }
    }

    fn get_appearance(&self) -> Appearance {
        match self {
            Self::X11(x) => x.get_appearance(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.get_appearance(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.get_appearance(),
        }
    }

    fn beep(&self) {
        match self {
            Self::X11(x) => x.beep(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.beep(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.beep(),
        }
    }

    fn screens(&self) -> anyhow::Result<Screens> {
        match self {
            Self::X11(x) => x.screens(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.screens(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.screens(),
        }
    }
}

impl Window {
    pub async fn new_window<F>(
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        Connection::get()
            .unwrap()
            .new_window(
                class_name,
                name,
                geometry,
                config,
                font_config,
                event_handler,
            )
            .await
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        match self {
            Self::X11(x) => x.display_handle(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.display_handle(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.display_handle(),
        }
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        match self {
            Self::X11(x) => x.window_handle(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.window_handle(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.window_handle(),
        }
    }
}

#[async_trait(?Send)]
impl WindowOps for Window {
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        match self {
            Self::X11(x) => x.enable_opengl().await,
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.enable_opengl().await,
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.enable_opengl().await,
        }
    }

    fn finish_frame(&self, frame: glium::Frame) -> anyhow::Result<()> {
        match self {
            Self::X11(x) => x.finish_frame(frame),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.finish_frame(frame),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.finish_frame(frame),
        }
    }

    fn close(&self) {
        match self {
            Self::X11(x) => x.close(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.close(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.close(),
        }
    }
    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        match self {
            Self::X11(x) => x.notify(t),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.notify(t),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.notify(t),
        }
    }

    fn hide(&self) {
        match self {
            Self::X11(x) => x.hide(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.hide(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.hide(),
        }
    }

    fn focus(&self) {
        match self {
            Self::X11(x) => x.focus(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.focus(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.focus(),
        }
    }

    fn toggle_fullscreen(&self) {
        match self {
            Self::X11(x) => x.toggle_fullscreen(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.toggle_fullscreen(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.toggle_fullscreen(),
        }
    }

    fn config_did_change(&self, config: &ConfigHandle) {
        match self {
            Self::X11(x) => x.config_did_change(config),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.config_did_change(config),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.config_did_change(config),
        }
    }

    fn show(&self) {
        match self {
            Self::X11(x) => x.show(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.show(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.show(),
        }
    }

    fn set_cursor(&self, cursor: Option<CursorIcon>) {
        match self {
            Self::X11(x) => x.set_cursor(cursor),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_cursor(cursor),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_cursor(cursor),
        }
    }

    fn invalidate(&self) {
        match self {
            Self::X11(x) => x.invalidate(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.invalidate(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.invalidate(),
        }
    }

    fn set_resize_increments(&self, incr: ResizeIncrement) {
        match self {
            Self::X11(x11) => x11.set_resize_increments(incr),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_resize_increments(incr),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_resize_increments(incr),
        }
    }

    fn set_title(&self, title: &str) {
        match self {
            Self::X11(x) => x.set_title(title),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_title(title),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_title(title),
        }
    }

    fn set_icon(&self, image: crate::bitmaps::Image) {
        match self {
            Self::X11(x) => x.set_icon(image),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_icon(image),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_icon(image),
        }
    }

    fn maximize(&self) {
        match self {
            Self::X11(x) => x.maximize(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.maximize(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.maximize(),
        }
    }

    fn restore(&self) {
        match self {
            Self::X11(x) => x.restore(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.restore(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.restore(),
        }
    }

    fn set_inner_size(&self, width: usize, height: usize) {
        match self {
            Self::X11(x) => x.set_inner_size(width, height),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_inner_size(width, height),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_inner_size(width, height),
        }
    }

    fn request_drag_move(&self) {
        match self {
            Self::X11(x) => x.request_drag_move(),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.request_drag_move(),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.request_drag_move(),
        }
    }

    fn set_window_drag_position(&self, coords: ScreenPoint) {
        match self {
            Self::X11(x) => x.set_window_drag_position(coords),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_window_drag_position(coords),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_window_drag_position(coords),
        }
    }

    fn set_window_position(&self, coords: ScreenPoint) {
        match self {
            Self::X11(x) => x.set_window_position(coords),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_window_position(coords),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_window_position(coords),
        }
    }

    fn set_text_cursor_position(&self, cursor: Rect) {
        match self {
            Self::X11(x) => x.set_text_cursor_position(cursor),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_text_cursor_position(cursor),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_text_cursor_position(cursor),
        }
    }

    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String> {
        match self {
            Self::X11(x) => x.get_clipboard(clipboard),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.get_clipboard(clipboard),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.get_clipboard(clipboard),
        }
    }
    fn set_clipboard(&self, clipboard: Clipboard, text: String) {
        match self {
            Self::X11(x) => x.set_clipboard(clipboard, text),
            #[cfg(feature = "wayland")]
            Self::Wayland(w) => w.set_clipboard(clipboard, text),
            #[cfg(feature = "gtk")]
            Self::Gtk(g) => g.set_clipboard(clipboard, text),
        }
    }
}
