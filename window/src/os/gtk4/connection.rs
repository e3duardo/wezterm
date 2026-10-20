use super::window::GtkWindowInner;
use crate::connection::ConnectionOps;
use crate::x11::{Keyboard, KeyboardWithFallback};
use crate::Connection;
use gdk4::prelude::*;
use glib::translate::ToGlibPtr;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use xkbcommon::xkb;

pub struct GtkConnection {
    main_loop: glib::MainLoop,
    next_window_id: AtomicUsize,
    pub(super) windows: RefCell<HashMap<usize, Rc<RefCell<GtkWindowInner>>>>,
    // Mapeador de teclado do próprio wezterm (layout, teclas mortas,
    // compose), o mesmo usado pelos backends X11/Wayland. Criado sob
    // demanda a partir do keymap xkb que o GDK recebeu do compositor; o
    // ponteiro guardado junto serve pra perceber quando o usuário troca de
    // layout (o GDK passa a ter outro keymap) e recriar o mapeador.
    keyboard: RefCell<Option<(usize, KeyboardWithFallback)>>,
}

impl GtkConnection {
    pub fn create_new() -> anyhow::Result<Self> {
        log::trace!("antes do gtk4::init()");
        let r = gtk4::init();
        log::trace!("depois do gtk4::init(), resultado={:?}", r);
        r.map_err(|e| anyhow::anyhow!("gtk4::init failed: {e}"))?;

        log::trace!("antes do timeout_add_local");
        // TODO(marco 4+): isso é só um polling simples da SPAWN_QUEUE, não
        // um watch de verdade no fd dela via GLib. Funciona, mas acorda o
        // loop a cada 8ms mesmo sem trabalho pendente - revisitar depois
        // com um g_source de fd de verdade.
        glib::timeout_add_local(std::time::Duration::from_millis(8), || {
            crate::spawn::SPAWN_QUEUE.run();
            glib::ControlFlow::Continue
        });
        log::trace!("depois do timeout_add_local, antes do MainLoop::new");

        let main_loop = glib::MainLoop::new(None, false);
        log::trace!("MainLoop criado");

        Ok(Self {
            main_loop,
            next_window_id: AtomicUsize::new(1),
            windows: RefCell::new(HashMap::new()),
            keyboard: RefCell::new(None),
        })
    }

    pub(crate) fn next_window_id(&self) -> usize {
        self.next_window_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn window_by_id(&self, id: usize) -> Option<Rc<RefCell<GtkWindowInner>>> {
        self.windows.borrow().get(&id).cloned()
    }

    pub(crate) fn register_window(&self, id: usize, inner: Rc<RefCell<GtkWindowInner>>) {
        self.windows.borrow_mut().insert(id, inner);
    }

    pub(crate) fn unregister_window(&self, id: usize) -> Option<Rc<RefCell<GtkWindowInner>>> {
        self.windows.borrow_mut().remove(&id)
    }

    pub(crate) fn with_window_inner<R>(
        window: usize,
        f: impl FnOnce(&mut GtkWindowInner) -> R,
    ) -> Option<R> {
        let conn = Connection::get()?.gtk();
        let inner = conn.window_by_id(window)?;
        let mut inner = inner.borrow_mut();
        Some(f(&mut inner))
    }

    /// Roda `f` com o mapeador de teclado, (re)criando ele se o keymap do
    /// GDK mudou desde a última vez (ou se ainda não existia).
    pub(crate) fn with_keyboard<R>(&self, f: impl FnOnce(&KeyboardWithFallback) -> R) -> Option<R> {
        let gdk_keymap = gdk_xkb_keymap();
        let mut keyboard = self.keyboard.borrow_mut();

        let stale = match (keyboard.as_ref(), gdk_keymap) {
            (None, _) => true,
            (Some((cached, _)), Some(ptr)) => *cached != ptr as usize,
            (Some(_), None) => false,
        };
        if stale {
            let built = match gdk_keymap {
                Some(ptr) => unsafe {
                    // O keymap pertence ao GDK: pegamos uma referência
                    // própria antes de embrulhar (o Keymap do xkbcommon
                    // solta uma referência ao ser destruído).
                    xkb::ffi::xkb_keymap_ref(ptr);
                    let keymap = xkb::Keymap::from_raw_ptr(ptr);
                    KeyboardWithFallback::new_from_string(
                        keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
                    )
                },
                None => Keyboard::new_default().and_then(KeyboardWithFallback::new),
            };
            match built {
                Ok(k) => {
                    *keyboard = Some((gdk_keymap.map(|p| p as usize).unwrap_or(0), k));
                }
                Err(e) => {
                    log::error!("falha ao criar mapeador de teclado: {e:#}");
                    return None;
                }
            }
        }

        keyboard.as_ref().map(|(_, k)| f(k))
    }
}

/// O keymap xkb que o GDK recebeu do compositor pro teclado do seat padrão.
fn gdk_xkb_keymap() -> Option<*mut xkb::ffi::xkb_keymap> {
    let device = gdk4::Display::default()?.default_seat()?.keyboard()?;
    let device = device.downcast::<gdk4_wayland::WaylandDevice>().ok()?;
    let ptr = unsafe { gdk4_wayland::ffi::gdk_wayland_device_get_xkb_keymap(device.to_glib_none().0) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr as *mut xkb::ffi::xkb_keymap)
    }
}

impl ConnectionOps for GtkConnection {
    fn name(&self) -> String {
        "Gtk".to_string()
    }

    fn terminate_message_loop(&self) {
        if self.main_loop.is_running() {
            self.main_loop.quit();
        }
    }

    fn run_message_loop(&self) -> anyhow::Result<()> {
        self.main_loop.run();
        Ok(())
    }
}
