use super::connection::GtkConnection;
use crate::{
    Clipboard, Connection, ConnectionOps, CursorIcon, Dimensions, MouseButtons, MouseEvent,
    MouseEventKind, MousePress, Point, Rect, RequestedWindowGeometry, ResolvedGeometry,
    ScreenPoint, Window, WindowEvent, WindowEventSender, WindowOps,
};
use anyhow::anyhow;
use async_trait::async_trait;
use config::ConfigHandle;
use gtk4::prelude::*;
use gdk4::prelude::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wezterm_font::FontConfiguration;

pub struct GtkWindowInner {
    // `events` é um handle PRÓPRIO (Rc<RefCell<..>>), separado do resto
    // do estado da janela. É assim de propósito: despachar um evento
    // chama de volta código do wezterm-gui que pode, de dentro desse
    // mesmo despacho, chamar outro método do WindowOps (ex: invalidate(),
    // set_title()) de forma síncrona e reentrante. Se `events` fosse só
    // mais um campo dentro do mesmo RefCell que `window`/`gl_area`/etc,
    // essa chamada reentrante ia tentar pegar emprestado de novo o MESMO
    // RefCell que o despacho de fora já está segurando, e panica
    // ("RefCell already borrowed"). Com `events` num RefCell separado, só
    // uma chamada reentrante que TAMBÉM despacha evento (não é o caso
    // comum) ainda esbarraria nisso - ver o comentário em `notify()`.
    pub(super) events: Rc<RefCell<WindowEventSender>>,
    window: gtk4::Window,
    gl_area: gtk4::GLArea,
    gl_context: Option<Rc<glium::backend::Context>>,
    dimensions: Dimensions,
    // Única fonte de verdade pro tamanho em pixels físicos do framebuffer,
    // escrita só pelo sinal "resize" do GLArea (que já entrega pixels
    // físicos prontos, sem precisar multiplicar por scale_factor de
    // novo). Compartilhada com GlAreaBackend pra ele nunca reconsultar o
    // tamanho por outro caminho (área.width()/height() lógicos × escala)
    // que podia dessincronizar durante um resize ativo.
    pixel_size: Rc<Cell<(u32, u32)>>,
    // O último tamanho que o wezterm-gui conhece (o da criação, ou o do
    // último Resized despachado). Os resizes que chegam antes do
    // TermWindow estar pronto não são despachados (ver connect_resize),
    // e a barra de título do GTK come parte da altura pedida - então logo
    // depois de abrir, `dimensions` (o real) costuma ser menor que isso.
    // O sinal render compara os dois e despacha o Resized que faltou.
    reported_dimensions: Dimensions,
    // Estado do mouse que o GTK não guarda pra gente: botões seguros e a
    // última posição (em pixels físicos). Cliques e rolagem usam a última
    // posição do movimento, igual ao backend Wayland.
    mouse_buttons: MouseButtons,
    last_mouse_coords: Point,
}

/// Backend do glium que entrega os comandos GL pro framebuffer que o
/// GtkGLArea já deixa pronto e corrente - a gente nunca chama
/// eglSwapBuffers, quem decide quando apresentar é o próprio GTK (é o
/// sinal "render" do GLArea que dispara WindowEvent::NeedRepaint, que por
/// sua vez faz o resto do wezterm desenhar via esse backend).
///
/// Descoberto na spike (~/vendor/gtk4-wl-subsurface-spike): desenhar
/// direto numa superfície Wayland nossa, com EGL próprio, dá
/// EGL_BAD_NATIVE_WINDOW sempre que compartilha a conexão com o GDK -
/// conflito entre o loop de eventos do GLib e o despacho interno do Mesa
/// durante o SwapBuffers. Essa abordagem (GtkGLArea) evita isso porque
/// nunca chamamos SwapBuffers nós mesmos.
struct GlAreaBackend {
    get_proc_address: unsafe extern "C" fn(*const std::os::raw::c_char) -> *const std::os::raw::c_void,
    _lib: &'static libloading::Library,
    // Mesmo Rc<Cell<>> que GtkWindowInner::pixel_size - só o sinal
    // "resize" escreve aqui, com os pixels físicos que ele mesmo recebeu
    // (ver comentário em GtkWindowInner::pixel_size).
    pixel_size: Rc<Cell<(u32, u32)>>,
}

unsafe impl glium::backend::Backend for GlAreaBackend {
    fn swap_buffers(&self) -> Result<(), glium::SwapBuffersError> {
        Ok(())
    }

    unsafe fn get_proc_address(&self, symbol: &str) -> *const std::ffi::c_void {
        let sym_name = std::ffi::CString::new(symbol).expect("symbol to be cstring compatible");
        (self.get_proc_address)(sym_name.as_ptr()) as *const std::ffi::c_void
    }

    fn get_framebuffer_dimensions(&self) -> (u32, u32) {
        self.pixel_size.get()
    }

    fn is_current(&self) -> bool {
        true
    }

    unsafe fn make_current(&self) {}

    fn resize(&self, _size: (u32, u32)) {
        // Não precisa fazer nada - get_framebuffer_dimensions() já
        // consulta o tamanho real do widget direto, sempre em dia.
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct GtkWindow(usize);

impl GtkWindow {
    pub async fn new_window<F>(
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let config = match config {
            Some(c) => c.clone(),
            None => config::configuration(),
        };

        log::trace!("GtkWindow::new_window início");
        let conn = Connection::get()
            .ok_or_else(|| anyhow!("new_window must be called after Connection::init"))?
            .gtk();
        log::trace!("conn obtida");

        let window_id = conn.next_window_id();

        let ResolvedGeometry {
            x: _,
            y: _,
            width,
            height,
        } = conn.resolve_geometry(geometry);

        let dimensions = Dimensions {
            pixel_width: width,
            pixel_height: height,
            dpi: config.dpi.unwrap_or(crate::DEFAULT_DPI) as usize,
        };

        log::trace!("antes de criar gtk4::Window");
        // No Wayland o GTK usa o prgname como app_id da janela. Tem que
        // bater com o .desktop (org.wezfurlong.wezterm) pro GNOME associar
        // a janela ao ícone/atalho do dock - senão vira um app "anônimo".
        glib::set_prgname(Some(class_name));
        let window = gtk4::Window::new();
        log::trace!("gtk4::Window criada");
        window.set_title(Some(name));
        window.set_default_size(width as i32, height as i32);
        window.set_decorated(decorated(&config));

        let gl_area = gtk4::GLArea::new();
        log::trace!("gtk4::GLArea criada");
        gl_area.set_has_depth_buffer(false);
        gl_area.set_auto_render(false);
        window.set_child(Some(&gl_area));


        let events = Rc::new(RefCell::new(WindowEventSender::new(event_handler)));
        let pixel_size = Rc::new(Cell::new((
            dimensions.pixel_width as u32,
            dimensions.pixel_height as u32,
        )));

        let inner = Rc::new(RefCell::new(GtkWindowInner {
            events: Rc::clone(&events),
            window: window.clone(),
            gl_area: gl_area.clone(),
            gl_context: None,
            dimensions,
            pixel_size: Rc::clone(&pixel_size),
            reported_dimensions: dimensions,
            mouse_buttons: MouseButtons::NONE,
            last_mouse_coords: Point::new(0, 0),
        }));

        let window_handle = Window::Gtk(GtkWindow(window_id));
        events.borrow_mut().assign_window(window_handle.clone());

        conn.register_window(window_id, Rc::clone(&inner));

        // Sinal "render": é aqui, e só aqui, que o GTK garante que o
        // contexto GL está corrente e o framebuffer certo está vinculado.
        // A primeira chamada também cria o contexto do glium (equivalente
        // ao "realize", mas fazer aqui garante que já está tudo pronto).
        {
            let window_id = window_id;
            gl_area.connect_render(move |area, _gl_ctx| {
                log::trace!("sinal render disparou");
                // Garante que o framebuffer certo do GLArea está
                // vinculado antes da gente desenhar - sem isso, estado
                // de GL deixado pelo próprio GTK (texturas/shaders dele)
                // pode vazar pro nosso desenho (foi exatamente o que
                // causou o conteúdo corrompido/em blocos coloridos).
                area.attach_buffers();
                let events = GtkConnection::with_window_inner(window_id, |inner| {
                    if inner.gl_context.is_none() {
                        log::trace!("render - criando contexto glium pela 1a vez");
                        match create_gl_context(Rc::clone(&inner.pixel_size)) {
                            Ok(ctx) => {
                                log::trace!("render - contexto glium criado OK");
                                inner.gl_context = Some(ctx);
                            }
                            Err(e) => {
                                log::trace!("render - falha ao criar contexto: {e:#}");
                                log::error!("falha ao criar contexto glium no GtkGLArea: {e:#}");
                            }
                        }
                        // Essa primeira chamada é só bootstrap do
                        // contexto (é o que `enable_opengl` está
                        // esperando) - não despachamos NeedRepaint aqui
                        // porque o TermWindow do wezterm-gui ainda não
                        // terminou de se montar nesse ponto (ainda está
                        // dentro do próprio `enable_opengl().await?`).
                        // Repaints de verdade entram via `invalidate()`
                        // (chamado pelo resto do wezterm-gui mais tarde).
                        return None;
                    }
                    let pending_resize = (inner.dimensions != inner.reported_dimensions)
                        .then(|| (inner.dimensions, window_state(&inner.window)));
                    inner.reported_dimensions = inner.dimensions;
                    Some((
                        Rc::clone(&inner.events),
                        Rc::clone(inner.gl_context.as_ref().unwrap()),
                        pending_resize,
                    ))
                })
                .flatten();
                // Despacha FORA do empréstimo de `inner` (ver comentário
                // em GtkWindowInner::events) - dispatch chama de volta
                // código do wezterm-gui que desenha de verdade, e esse
                // código pode chamar outros métodos do WindowOps.
                if let Some((events, gl_context, pending_resize)) = events {
                    if let Some((dimensions, window_state)) = pending_resize {
                        events.borrow_mut().dispatch(WindowEvent::Resized {
                            dimensions,
                            window_state,
                            live_resizing: false,
                        });
                    }
                    // Ver Context::invalidate_texture_binding_cache (fork
                    // local do glium em deps/glium) - o GTK pode ter
                    // vinculado as próprias texturas nesse mesmo contexto
                    // GL desde o nosso último quadro, e o glium não sabe
                    // disso. Sem isso, o conteúdo sai corrompido (quadros
                    // de cor aleatória) - só "corrige" por acaso quando um
                    // redimensionamento grande força o GTK a recriar
                    // recursos internos.
                    unsafe {
                        gl_context.invalidate_texture_binding_cache();
                    }
                    log::trace!("render - despachando NeedRepaint");
                    events.borrow_mut().dispatch(WindowEvent::NeedRepaint);
                    log::trace!("render - NeedRepaint despachado e retornou");
                }
                glib::Propagation::Stop
            });
        }

        {
            let window_id = window_id;
            gl_area.connect_resize(move |_area, w, h| {
                log::trace!("sinal resize disparou w={w} h={h}");
                let dispatch = GtkConnection::with_window_inner(window_id, |inner| {
                    inner.dimensions.pixel_width = w.max(1) as usize;
                    inner.dimensions.pixel_height = h.max(1) as usize;
                    inner.pixel_size.set((w.max(1) as u32, h.max(1) as u32));
                    // O sinal "resize" dispara antes do primeiro "render"
                    // (é preciso ter um tamanho pra poder desenhar) - se
                    // despachar Resized aqui antes do contexto GL existir,
                    // o resto do wezterm-gui reage e tenta desenhar um
                    // TermWindow que ainda não terminou de se montar,
                    // deixando o primeiro quadro incompleto (tela branca/
                    // cores erradas até redimensionar de verdade). Só
                    // despacha depois que o bootstrap do contexto já
                    // aconteceu.
                    if inner.gl_context.is_none() {
                        return None;
                    }
                    inner.reported_dimensions = inner.dimensions;
                    Some((
                        Rc::clone(&inner.events),
                        inner.dimensions,
                        window_state(&inner.window),
                    ))
                })
                .flatten();
                if let Some((events, dimensions, window_state)) = dispatch {
                    events.borrow_mut().dispatch(WindowEvent::Resized {
                        dimensions,
                        window_state,
                        live_resizing: false,
                    });
                }
            });
        }

        // Botão de fechar da barra nativa: quem decide é o wezterm-gui
        // (pode pedir confirmação), então barramos o fechamento do GTK e só
        // avisamos. Se ele aceitar, chama close() (ver WindowOps::close).
        {
            let window_id = window_id;
            window.connect_close_request(move |_window| {
                let events =
                    GtkConnection::with_window_inner(window_id, |inner| Rc::clone(&inner.events));
                if let Some(events) = events {
                    events.borrow_mut().dispatch(WindowEvent::CloseRequested);
                }
                glib::Propagation::Stop
            });
        }

        // Teclado: no Wayland, as teclas vão pra quem é dono da janela -
        // agora o GTK. Pegamos só o keycode bruto (fase Capture + Stop,
        // pra o GTK não usar Tab/F10/etc pra navegação própria) e
        // entregamos pro mesmo mapeador xkb que o backend Wayland usa.
        // Repetição de tecla segurada já vem pronta do GDK (key-pressed
        // dispara de novo), então não precisamos do timer de repeat.
        {
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let window_id = window_id;
            key_ctrl.connect_key_pressed(move |_, _keyval, keycode, _state| {
                handle_key(window_id, keycode, true);
                glib::Propagation::Stop
            });
            key_ctrl.connect_key_released(move |_, _keyval, keycode, _state| {
                handle_key(window_id, keycode, false);
            });
            window.add_controller(key_ctrl);
        }

        // Mouse: movimento, botões e rolagem chegam no GLArea. Os botões
        // vêm do EventControllerLegacy (eventos crus) e não do GestureClick,
        // que se cancela quando o ponteiro anda além do limiar de arrasto -
        // aí o "released" nunca chegava e o botão ficava preso no meio de
        // uma seleção. Durante o arrasto o GTK mantém o grab implícito no
        // GLArea, então o movimento continua chegando aqui.
        {
            let motion = gtk4::EventControllerMotion::new();
            let window_id = window_id;
            motion.connect_motion(move |_, x, y| {
                send_mouse(window_id, Some((x, y)), MouseEventKind::Move, None);
            });
            motion.connect_leave(move |_| {
                let events =
                    GtkConnection::with_window_inner(window_id, |inner| Rc::clone(&inner.events));
                if let Some(events) = events {
                    events.borrow_mut().dispatch(WindowEvent::MouseLeave);
                }
            });
            gl_area.add_controller(motion);

            let buttons = gtk4::EventControllerLegacy::new();
            buttons.connect_event(move |_, event| {
                let pressed = match event.event_type() {
                    gdk4::EventType::ButtonPress => true,
                    gdk4::EventType::ButtonRelease => false,
                    _ => return glib::Propagation::Proceed,
                };
                let Some(button) = event.downcast_ref::<gdk4::ButtonEvent>() else {
                    return glib::Propagation::Proceed;
                };
                let (press, mask) = match button.button() {
                    1 => (MousePress::Left, MouseButtons::LEFT),
                    2 => (MousePress::Middle, MouseButtons::MIDDLE),
                    3 => (MousePress::Right, MouseButtons::RIGHT),
                    _ => return glib::Propagation::Proceed,
                };
                let kind = if pressed {
                    MouseEventKind::Press(press)
                } else {
                    MouseEventKind::Release(press)
                };
                send_mouse(window_id, None, kind, Some((mask, pressed)));
                glib::Propagation::Stop
            });
            gl_area.add_controller(buttons);

            // DISCRETE: roda do mouse já vem em "dentes" (±1), e o touchpad
            // é acumulado pelo GTK até completar um passo.
            let scroll = gtk4::EventControllerScroll::new(
                gtk4::EventControllerScrollFlags::BOTH_AXES
                    | gtk4::EventControllerScrollFlags::DISCRETE,
            );
            scroll.connect_scroll(move |_, dx, dy| {
                // No GTK, positivo é pra baixo/direita; no wezterm é o contrário.
                if dy != 0.0 {
                    send_mouse(window_id, None, MouseEventKind::VertWheel(-dy as i16), None);
                }
                if dx != 0.0 {
                    send_mouse(window_id, None, MouseEventKind::HorzWheel(-dx as i16), None);
                }
                glib::Propagation::Stop
            });
            gl_area.add_controller(scroll);
        }

        // Arrastar arquivos pra dentro: o wezterm-gui cola os caminhos
        // (ou faz o que `quote_dropped_files` mandar).
        {
            let drop = gtk4::DropTarget::new(
                gdk4::FileList::static_type(),
                gdk4::DragAction::COPY,
            );
            let window_id = window_id;
            drop.connect_drop(move |_, value, _, _| {
                let Ok(files) = value.get::<gdk4::FileList>() else {
                    return false;
                };
                let paths: Vec<std::path::PathBuf> =
                    files.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() {
                    return false;
                }
                let events =
                    GtkConnection::with_window_inner(window_id, |inner| Rc::clone(&inner.events));
                if let Some(events) = events {
                    events.borrow_mut().dispatch(WindowEvent::DroppedFile(paths));
                }
                true
            });
            gl_area.add_controller(drop);
        }

        // Foco: "is-active" é o foco da janela inteira (toplevel). Ao
        // trocar de foco zeramos os modificadores (igual ao backend
        // Wayland), senão um Ctrl+Shift seguro no momento em que outra
        // janela ganhou o foco ficaria "preso" pra sempre.
        {
            let window_id = window_id;
            window.connect_is_active_notify(move |w| {
                let focused = w.is_active();
                if let Some(conn) = Connection::get() {
                    let (caps, num) = gdk4::Display::default()
                        .and_then(|d| d.default_seat())
                        .and_then(|s| s.keyboard())
                        .map(|k| (k.is_caps_locked(), k.is_num_locked()))
                        .unwrap_or((false, false));
                    conn.gtk()
                        .with_keyboard(|mapper| mapper.reset_modifiers_with_locks(caps, num));
                }
                let events =
                    GtkConnection::with_window_inner(window_id, |inner| Rc::clone(&inner.events));
                if let Some(events) = events {
                    events
                        .borrow_mut()
                        .dispatch(WindowEvent::FocusChanged(focused));
                }
            });
        }

        log::trace!("antes de window.present()");
        window.present();
        log::trace!("depois de window.present()");

        Ok(window_handle)
    }
}

/// Entrega uma tecla pro mapeador xkb do wezterm. `keycode` é o keycode
/// de hardware do GDK, que no Wayland é o código evdev + 8 (o mesmo que
/// o xkb usa); `process_wayland_key` espera o evdev puro.
fn handle_key(window_id: usize, keycode: u32, pressed: bool) {
    let Some(conn) = Connection::get() else {
        return;
    };
    let Some(events) =
        GtkConnection::with_window_inner(window_id, |inner| Rc::clone(&inner.events))
    else {
        return;
    };
    let code = keycode.saturating_sub(8);

    let changed_mods = conn.gtk().with_keyboard(|mapper| {
        // Primeiro processa a tecla com o estado de antes dela (é a ordem
        // do protocolo Wayland: o evento Key chega antes do Modifiers que
        // ela causa), depois atualiza o estado dos modificadores.
        mapper.process_wayland_key(code, pressed, &mut events.borrow_mut());

        let before = (mapper.get_key_modifiers(), mapper.get_led_status());
        mapper.update_key_state(code, pressed);
        let after = (mapper.get_key_modifiers(), mapper.get_led_status());
        (before != after).then_some(after)
    });

    if let Some(Some((mods, leds))) = changed_mods {
        events
            .borrow_mut()
            .dispatch(WindowEvent::AdviseModifiersLedStatus(mods, leds));
    }
}

/// A barra de título nativa do GTK só aparece com TITLE em
/// `window_decorations`; sem ela a janela fica sem decoração nenhuma.
fn decorated(config: &ConfigHandle) -> bool {
    config
        .window_decorations
        .contains(crate::WindowDecorations::TITLE)
}

fn window_state(window: &gtk4::Window) -> crate::WindowState {
    let mut state = crate::WindowState::default();
    if window.is_maximized() {
        state |= crate::WindowState::MAXIMIZED;
    }
    if window.is_fullscreen() {
        state |= crate::WindowState::FULL_SCREEN;
    }
    state
}

/// A gtk4::Window, clonada pra fora do empréstimo de `inner`: maximizar,
/// tela cheia, decoração etc. podem fazer o GTK realocar o tamanho na
/// hora, e o sinal "resize" pegaria `inner` emprestado de novo.
fn gtk_window(window_id: usize) -> Option<gtk4::Window> {
    GtkConnection::with_window_inner(window_id, |inner| inner.window.clone())
}

/// Despacha um evento de mouse. `pos` vem em coordenadas lógicas do GTK,
/// relativas ao GLArea; o wezterm quer pixels físicos. A razão sai do
/// tamanho físico que o sinal "resize" entregou, então vale pra escala
/// fracionária também. Sem `pos`, usa a última posição conhecida.
fn send_mouse(
    window_id: usize,
    pos: Option<(f64, f64)>,
    kind: MouseEventKind,
    button: Option<(MouseButtons, bool)>,
) {
    let Some(conn) = Connection::get() else {
        return;
    };
    let modifiers = conn
        .gtk()
        .with_keyboard(|mapper| mapper.get_key_modifiers())
        .unwrap_or_default();
    let dispatch = GtkConnection::with_window_inner(window_id, |inner| {
        if let Some((x, y)) = pos {
            let scale =
                inner.dimensions.pixel_width as f64 / inner.gl_area.width().max(1) as f64;
            inner.last_mouse_coords = Point::new((x * scale) as isize, (y * scale) as isize);
        }
        if let Some((mask, pressed)) = button {
            if pressed {
                inner.mouse_buttons |= mask;
            } else {
                inner.mouse_buttons -= mask;
            }
        }
        let coords = inner.last_mouse_coords;
        let event = MouseEvent {
            kind,
            coords,
            // Igual ao backend Wayland: o Wayland não revela a posição da
            // janela na tela, então isso é só uma aproximação.
            screen_coords: ScreenPoint::new(
                coords.x + inner.dimensions.pixel_width as isize,
                coords.y + inner.dimensions.pixel_height as isize,
            ),
            mouse_buttons: inner.mouse_buttons,
            modifiers,
        };
        (Rc::clone(&inner.events), event)
    });
    if let Some((events, event)) = dispatch {
        events.borrow_mut().dispatch(WindowEvent::MouseEvent(event));
    }
}

/// O clipboard do GDK correspondente (CLIPBOARD ou a seleção PRIMARY, a
/// do clique do meio).
fn gdk_clipboard(clipboard: Clipboard) -> Option<gdk4::Clipboard> {
    let display = gdk4::Display::default()?;
    Some(match clipboard {
        Clipboard::Clipboard => display.clipboard(),
        Clipboard::PrimarySelection => display.primary_clipboard(),
    })
}

fn create_gl_context(
    pixel_size: Rc<Cell<(u32, u32)>>,
) -> anyhow::Result<Rc<glium::backend::Context>> {
    let lib: &'static libloading::Library = Box::leak(Box::new(
        unsafe { libloading::Library::new("libEGL.so.1") }
            .map_err(|e| anyhow!("carregar libEGL.so.1: {e}"))?,
    ));
    let get_proc_address: libloading::Symbol<
        unsafe extern "C" fn(*const std::os::raw::c_char) -> *const std::os::raw::c_void,
    > = unsafe { lib.get(b"eglGetProcAddress\0") }
        .map_err(|e| anyhow!("resolver eglGetProcAddress: {e}"))?;
    let get_proc_address = *get_proc_address;

    let backend = GlAreaBackend {
        get_proc_address,
        _lib: lib,
        pixel_size,
    };

    unsafe {
        glium::backend::Context::new(
            Rc::new(backend),
            false,
            glium::debug::DebugCallbackBehavior::DebugMessageOnError,
        )
    }
    .map_err(|e| anyhow!("glium::backend::Context::new: {e}"))
}

/// Fecha de verdade (o wezterm-gui já decidiu - ver WindowOps::close).
///
/// A janela sai do mapa e é destruída na hora: o wezterm-gui pode encerrar
/// o loop logo em seguida (última janela), e uma janela que ficasse no
/// mapa seria solta só no shutdown, com o TermWindow dropando dentro do
/// borrow_mut do FRONT_END (panic). Já o Destroyed é adiado, porque
/// normalmente estamos dentro do despacho do CloseRequested, segurando o
/// RefCell de `events`. Sem o Destroyed o wezterm-gui nunca solta a
/// janela: o processo, o pty e o programa dentro dele (zellij) ficavam
/// vivos escondidos, e as janelas novas iam parar nesse processo fantasma.
fn close_window(window: usize) {
    let Some(conn) = Connection::get() else {
        return;
    };
    let Some(inner) = conn.gtk().unregister_window(window) else {
        return;
    };
    let (events, gtk_window) = {
        let inner = inner.borrow();
        (Rc::clone(&inner.events), inner.window.clone())
    };
    gtk_window.destroy();
    glib::idle_add_local_once(move || {
        events.borrow_mut().dispatch(WindowEvent::Destroyed);
    });
}

#[async_trait(?Send)]
impl WindowOps for GtkWindow {
    fn show(&self) {
        GtkConnection::with_window_inner(self.0, |inner| inner.window.present());
    }

    fn hide(&self) {
        GtkConnection::with_window_inner(self.0, |inner| inner.window.set_visible(false));
    }

    fn close(&self) {
        let window = self.0;
        if glib::MainContext::default().is_owner() {
            close_window(window);
        } else {
            promise::spawn::spawn_into_main_thread(async move { close_window(window) }).detach();
        }
    }

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        // Adiado (idle, não síncrono): notify() costuma ser chamado DE
        // DENTRO do despacho de outro evento (ex: apply_dimensions, ao
        // reagir a um Resized, chama notify() pra reaplicar config). Isso
        // significaria despachar de novo enquanto o despacho de fora
        // ainda está em andamento e segurando o RefCell de `events` -
        // panica ("RefCell already borrowed"). Adiando pro próximo giro
        // do loop do GLib, o despacho de fora já terminou e soltou o
        // empréstimo.
        //
        // notify() também é chamado de outras threads (ex: a resolução de
        // fonte de fallback do wezterm-font roda em background), e o
        // idle_add_local_once panica fora da thread dona do MainContext.
        // Por isso primeiro passamos pra thread principal via SPAWN_QUEUE,
        // igual ao backend Wayland, e só lá agendamos o idle.
        let window = self.0;
        promise::spawn::spawn_into_main_thread(async move {
            glib::idle_add_local_once(move || {
                let events =
                    GtkConnection::with_window_inner(window, |inner| Rc::clone(&inner.events));
                if let Some(events) = events {
                    events
                        .borrow_mut()
                        .dispatch(WindowEvent::Notification(Box::new(t)));
                }
            });
        })
        .detach();
    }

    fn set_cursor(&self, cursor: Option<CursorIcon>) {
        // Os nomes do cursor-icon são os nomes CSS, os mesmos que o GDK
        // aceita; None esconde o cursor (ex: enquanto digita).
        let name = cursor.map(|c| c.name()).unwrap_or("none");
        let area = GtkConnection::with_window_inner(self.0, |inner| inner.gl_area.clone());
        if let Some(area) = area {
            area.set_cursor_from_name(Some(name));
        }
    }

    fn invalidate(&self) {
        GtkConnection::with_window_inner(self.0, |inner| inner.gl_area.queue_render());
    }

    fn set_title(&self, title: &str) {
        let title = title.to_string();
        GtkConnection::with_window_inner(self.0, move |inner| inner.window.set_title(Some(&title)));
    }

    fn set_inner_size(&self, width: usize, height: usize) {
        GtkConnection::with_window_inner(self.0, move |inner| {
            inner.window.set_default_size(width as i32, height as i32)
        });
    }

    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        log::trace!("enable_opengl início");
        let window = self.0;
        let main_context = glib::MainContext::default();
        let mut iter = 0;
        // O contexto só existe depois do primeiro sinal "render" do
        // GLArea, e esse sinal só dispara se alguém bombear o laço de
        // eventos do GLib - nesse ponto do startup do wezterm-gui, o
        // `run_message_loop` de verdade ainda não começou a rodar. Em vez
        // de usar o timer do async_io (que não tem nada a ver com o GLib
        // e causava um crash aqui), bombeamos o MainContext manualmente.
        loop {
            let ctx = GtkConnection::with_window_inner(window, |inner| inner.gl_context.clone())
                .flatten();
            if let Some(ctx) = ctx {
                log::trace!("enable_opengl obteve contexto depois de {iter} iterações");
                return Ok(ctx);
            }
            if iter % 200 == 0 {
                log::trace!("enable_opengl ainda esperando, iteração {iter}");
            }
            iter += 1;
            if iter > 20_000 {
                log::trace!("enable_opengl desistindo depois de {iter} iterações");
                anyhow::bail!("timeout esperando o contexto GL do GtkGLArea");
            }
            main_context.iteration(true);
        }
    }

    fn get_clipboard(&self, clipboard: Clipboard) -> promise::Future<String> {
        let mut promise = promise::Promise::new();
        let future = promise.get_future().unwrap();
        // O GDK só pode ser usado na thread principal, e a leitura dele é
        // um future que não é Send: passa pra thread principal pela
        // SPAWN_QUEUE e roda a leitura no MainContext do GLib.
        promise::spawn::spawn_into_main_thread(async move {
            glib::MainContext::default().spawn_local(async move {
                let Some(gdk_clipboard) = gdk_clipboard(clipboard) else {
                    promise.err(anyhow!("sem display GDK"));
                    return;
                };
                let _ = match gdk_clipboard.read_text_future().await {
                    // Igual ao backend Wayland: normaliza pra fim de linha
                    // unix (copiar do Firefox etc. traz \r\n).
                    Ok(Some(text)) => promise.ok(text.replace("\r\n", "\n")),
                    Ok(None) => promise.ok(String::new()),
                    Err(e) => promise.err(anyhow!("lendo clipboard: {e}")),
                };
            });
        })
        .detach();
        future
    }

    fn set_clipboard(&self, clipboard: Clipboard, text: String) {
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(gdk_clipboard) = gdk_clipboard(clipboard) {
                gdk_clipboard.set_text(&text);
            }
        })
        .detach();
    }

    fn maximize(&self) {
        if let Some(window) = gtk_window(self.0) {
            window.maximize();
        }
    }

    fn restore(&self) {
        if let Some(window) = gtk_window(self.0) {
            if window.is_fullscreen() {
                window.unfullscreen();
            }
            window.unmaximize();
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = gtk_window(self.0) {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        }
    }

    fn focus(&self) {
        if let Some(window) = gtk_window(self.0) {
            window.present();
        }
    }

    fn config_did_change(&self, config: &ConfigHandle) {
        let decorated = decorated(config);
        if let Some(window) = gtk_window(self.0) {
            window.set_decorated(decorated);
        }
    }

    fn set_window_position(&self, _coords: ScreenPoint) {}

    fn set_text_cursor_position(&self, _cursor: Rect) {}
}

impl HasWindowHandle for GtkWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasDisplayHandle for GtkWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

#[allow(dead_code)]
fn unused_point(_: Point) {}
