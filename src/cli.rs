//! Access to the standard `netsblox-vm` CLI.
//!
//! Access to this submodule requires the `cli` feature flag, which is enabled by default.
//!
//! This submodule acts as a full-fledged implementation of a usable `netsblox-vm` CLI front-end.
//! This includes being able to compile and run individual project files locally,
//! as well as a server mode where a user can connect to the server from the browser
//! and use the block-based interface to write, upload, and run code on the server.
//! Note that server mode does not yet support multiple simultaneous.

use alloc::rc::Rc;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::collections::VecDeque;

use core::time::Duration;
use core::cell::{Cell, RefCell};
use core::{mem, fmt};

use std::fs::File;
use std::io::{self, Read, Write as IoWrite, stdout};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::atomic::{AtomicBool, Ordering as MemoryOrder};
use std::thread;

use clap::Subcommand;
use actix_web::{get, post, web, App, HttpServer, Responder, HttpResponse};
use actix_cors::Cors;

use crossterm::{cursor, execute, queue};
use crossterm::tty::IsTty;
use crossterm::event::{self, Event, KeyCode as RawKeyCode, KeyModifiers as RawKeyModifiers};
use crossterm::terminal::{self, ClearType};
use crossterm::style::{ResetColor, SetForegroundColor, Color, Print};

use crate::*;
use crate::gc::*;
use crate::json::*;
use crate::real_time::*;
use crate::std_system::*;
use crate::bytecode::*;
use crate::runtime::*;
use crate::process::*;
use crate::project::*;
use crate::template::*;

const DEFAULT_BASE_URL: &str = "https://cloud.netsblox.org";
const STEPS_PER_IO_ITER: usize = 64;
const MAX_REQUEST_SIZE_BYTES: usize = 1024 * 1024 * 1024;
const YIELDS_BEFORE_IDLE_SLEEP: usize = 256;
const IDLE_SLEEP_TIME: Duration = Duration::from_micros(500);

macro_rules! crash {
    ($ret:literal : $($tt:tt)*) => {{
        eprint!($($tt)*);
        eprint!("\r\n");
        std::process::exit($ret);
    }}
}

struct AtExit<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> AtExit<F> {
    fn new(f: F) -> Self { Self(Some(f)) }
}
impl<F: FnOnce()> Drop for AtExit<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()()
    }
}

#[derive(Collect)]
#[collect(no_drop, bound = "")]
struct Env<'gc, C: CustomTypes<StdSystem<C>>> {
                               proj: Gc<'gc, RefLock<Project<'gc, C, StdSystem<C>>>>,
    #[collect(require_static)] locs: Locations,
}
type EnvArena<S> = Arena<Rootable![Env<'_, S>]>;

fn get_env<C: CustomTypes<StdSystem<C>>>(role: &ast::Role, system: Rc<StdSystem<C>>) -> Result<EnvArena<C>, FromAstError> {
    let (bytecode, init_info, locs, _) = ByteCode::compile(role)?;
    Ok(EnvArena::new(Default::default(), |mc| {
        let proj = Project::from_init(mc, &init_info, Rc::new(bytecode), Settings::default(), system);
        Env { proj: Gc::new(mc, RefLock::new(proj)), locs }
    }))
}

/// Standard NetsBlox VM project actions that can be performed
#[derive(Subcommand)]
pub enum Mode {
    /// Compiles and runs a single project file
    Run {
        /// Path to the (xml) project file
        src: String,
        /// The specific role to run, or none if not ambiguous
        #[clap(long)]
        role: Option<String>,

        /// Address of the NetsBlox server
        #[clap(long, default_value_t = String::from(DEFAULT_BASE_URL))]
        server: String,
    },
    /// Compiles a single project file and dumps its disassembly to stdout
    Dump {
        /// Path to the (xml) project file
        src: String,
        /// The specific role to compile, or none if not ambiguous
        #[clap(long)]
        role: Option<String>,
    },
    /// Starts an execution server which you can connect to from the browser
    Start {
        /// Address of the NetsBlox server
        #[clap(long, default_value_t = String::from(DEFAULT_BASE_URL))]
        server: String,

        /// The address of this machine, which others use to send HTTP requests
        #[clap(long, default_value_t = String::from("127.0.0.1"))]
        addr: String,
        /// The port to bind for the web server
        #[clap(long, default_value_t = 6286)]
        port: u16,
    },
}

#[derive(Debug)]
enum OpenProjectError<'a> {
    ParseError { error: Box<ast::Error> },
    RoleNotFound { role: &'a str },
    NoRoles,
    MultipleRoles { count: usize },
}
impl fmt::Display for OpenProjectError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenProjectError::ParseError { error } => write!(f, "failed to parse project: {error:?}"),
            OpenProjectError::RoleNotFound { role } => write!(f, "no role named '{role}'"),
            OpenProjectError::NoRoles => write!(f, "project had no roles"),
            OpenProjectError::MultipleRoles { count } => write!(f, "project had multiple ({count}) roles, but a specific role was not specified"),
        }
    }
}

fn read_file(src: &str) -> io::Result<String> {
    let mut file = File::open(src)?;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(s)
}
fn open_project<'a>(content: &str, role: Option<&'a str>) -> Result<(String, ast::Role), OpenProjectError<'a>> {
    let parsed = match ast::Parser::default().parse(content) {
        Ok(x) => x,
        Err(error) => return Err(OpenProjectError::ParseError { error }),
    };
    let role = match role {
        Some(role) => match parsed.roles.into_iter().find(|x| x.name == role) {
            Some(x) => x,
            None => return Err(OpenProjectError::RoleNotFound { role }),
        }
        None => match parsed.roles.len() {
            0 => return Err(OpenProjectError::NoRoles),
            1 => parsed.roles.into_iter().next().unwrap(),
            count => return Err(OpenProjectError::MultipleRoles { count }),
        }
    };
    Ok((parsed.name, role))
}

fn run_proj_tty<C: CustomTypes<StdSystem<C>>>(project_name: &str, server: String, role: &ast::Role, overrides: Config<C, StdSystem<C>>, utc_offset: UtcOffset) {
    terminal::enable_raw_mode().unwrap();
    execute!(stdout(), cursor::Hide).unwrap();
    let _tty_mode_guard = AtExit::new(|| {
        terminal::disable_raw_mode().unwrap();
        execute!(stdout(), cursor::Show).unwrap()
    });

    let old_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |ctx| {
        let _ = terminal::disable_raw_mode();
        old_panic_hook(ctx);
    }));

    let update_flag = Rc::new(Cell::new(false));
    let input_queries = Rc::new(RefCell::new(VecDeque::new()));
    let mut term_size = terminal::size().unwrap();
    let mut input_value = String::new();

    let config = overrides.fallback(&Config {
        command: {
            let update_flag = update_flag.clone();
            Some(Rc::new(move |_, _, key, command, entity| match command {
                Command::Print { style: _, value } => {
                    if let Some(value) = value {
                        print!("{entity:?} > {value:?}\r\n");
                        update_flag.set(true);
                    }
                    key.complete(Ok(()));
                    CommandStatus::Handled
                }
                _ => CommandStatus::UseDefault { key, command },
            }))
        },
        request: {
            let update_flag = update_flag.clone();
            let input_queries = input_queries.clone();
            Some(Rc::new(move |_, _, key, request, entity| match request {
                Request::Input { prompt } => {
                    input_queries.borrow_mut().push_back((format!("{entity:?} {prompt:?} > "), key));
                    update_flag.set(true);
                    RequestStatus::Handled
                }
                _ => RequestStatus::UseDefault { key, request },
            }))
        },
    });

    let system = Rc::new(StdSystem::new_sync(server, Some(project_name), config, utc_offset));
    let mut idle_sleeper = IdleAction::new(YIELDS_BEFORE_IDLE_SLEEP, Box::new(|| thread::sleep(IDLE_SLEEP_TIME)));
    print!("public id: {}\r\n", system.get_public_id());

    let env = match get_env(role, system) {
        Ok(x) => x,
        Err(e) => {
            print!("error loading project: {e:?}\r\n");
            return;
        }
    };
    env.mutate(|mc, env| env.proj.borrow_mut(mc).input(mc, Input::Start));

    let mut input_sequence = Vec::with_capacity(16);
    let in_input_mode = || !input_queries.borrow().is_empty();
    'program: loop {
        debug_assert_eq!(input_sequence.len(), 0);
        while event::poll(Duration::from_secs(0)).unwrap() {
            match event::read().unwrap() {
                Event::Key(key) => match key.code {
                    RawKeyCode::Char('c') if key.modifiers == RawKeyModifiers::CONTROL => break 'program,
                    RawKeyCode::Esc => input_sequence.push(Input::Stop),
                    RawKeyCode::Char(ch) => match in_input_mode() {
                        true => { input_value.push(ch); update_flag.set(true); }
                        false => input_sequence.push(Input::KeyDown { key: KeyCode::Char(ch.to_ascii_lowercase()) }),
                    }
                    RawKeyCode::Backspace => if in_input_mode() && input_value.pop().is_some() { update_flag.set(true) }
                    RawKeyCode::Enter => if let Some((_, res_key)) = input_queries.borrow_mut().pop_front() {
                        res_key.complete(Ok(C::Intermediate::from_json(Json::String(mem::take(&mut input_value)))));
                        update_flag.set(true);
                    }
                    RawKeyCode::Up => if !in_input_mode() { input_sequence.push(Input::KeyDown { key: KeyCode::Up }) }
                    RawKeyCode::Down => if !in_input_mode() { input_sequence.push(Input::KeyDown { key: KeyCode::Down }) }
                    RawKeyCode::Left => if !in_input_mode() { input_sequence.push(Input::KeyDown { key: KeyCode::Left }) }
                    RawKeyCode::Right => if !in_input_mode() { input_sequence.push(Input::KeyDown { key: KeyCode::Right }) }
                    _ => (),
                }
                Event::Resize(c, r) => {
                    term_size = (c, r);
                    update_flag.set(true);
                }
                _ => (),
            }
        }

        env.mutate(|mc, env| {
            let mut proj = env.proj.borrow_mut(mc);
            for input in input_sequence.drain(..) { proj.input(mc, input); }
            for _ in 0..STEPS_PER_IO_ITER {
                let res = proj.step(mc);
                if let ProjectStep::Error { error, proc } = &res {
                    print!("\r\n>>> runtime error in entity {:?}: {:?}\r\n\r\n", proc.get_call_stack().last().unwrap().entity.borrow().name, error.cause);
                }
                idle_sleeper.consume(&res);
            }
        });

        if update_flag.get() {
            update_flag.set(false);

            queue!(stdout(),
                cursor::SavePosition,
                cursor::MoveTo(0, term_size.1 - 1),
                terminal::Clear(ClearType::CurrentLine)).unwrap();
            let queries = input_queries.borrow();
            if let Some((query, _)) = queries.front() {
                queue!(stdout(),
                    SetForegroundColor(Color::Blue),
                    Print(query),
                    ResetColor,
                    Print(&input_value)).unwrap();
            }
            queue!(stdout(), cursor::RestorePosition).unwrap();
            stdout().flush().unwrap();
        }
    }

    execute!(stdout(), terminal::Clear(ClearType::CurrentLine)).unwrap();
}
fn run_proj_non_tty<C: CustomTypes<StdSystem<C>>>(project_name: &str, server: String, role: &ast::Role, overrides: Config<C, StdSystem<C>>, utc_offset: UtcOffset) {
    let config = overrides.fallback(&Config {
        request: None,
        command: Some(Rc::new(move |_, _, key, command, entity| match command {
            Command::Print { style: _, value } => {
                if let Some(value) = value { println!("{entity:?} > {value:?}") }
                key.complete(Ok(()));
                CommandStatus::Handled
            }
            _ => CommandStatus::UseDefault { key, command },
        })),
    });

    let system = Rc::new(StdSystem::new_sync(server, Some(project_name), config, utc_offset));
    let mut idle_sleeper = IdleAction::new(YIELDS_BEFORE_IDLE_SLEEP, Box::new(|| thread::sleep(IDLE_SLEEP_TIME)));
    println!(">>> public id: {}\n", system.get_public_id());

    let env = match get_env(role, system) {
        Ok(x) => x,
        Err(e) => {
            println!(">>> error loading project: {e:?}");
            return;
        }
    };
    env.mutate(|mc, env| env.proj.borrow_mut(mc).input(mc, Input::Start));

    loop {
        env.mutate(|mc, env| {
            let mut proj = env.proj.borrow_mut(mc);
            for _ in 0..STEPS_PER_IO_ITER {
                let res = proj.step(mc);
                if let ProjectStep::Error { error, proc } = &res {
                    println!("\n>>> runtime error in entity {:?}: {:?}\n", proc.get_call_stack().last().unwrap().entity.borrow().name, error.cause);
                }
                idle_sleeper.consume(&res);
            }
        });
    }
}
fn run_server<C: CustomTypes<StdSystem<C>>>(nb_server: String, addr: String, port: u16, overrides: Config<C, StdSystem<C>>, utc_offset: UtcOffset, syscalls: &[SyscallMenu]) {
    println!(r#"connect from {nb_server}/?extensions=["http://{addr}:{port}/extension.js"]"#);

    let extension = ExtensionArgs {
        server: &format!("http://{addr}:{port}"),
        syscalls,
        omitted_elements: &["thumbnail", "pentrails", "history", "replay"],
        pull_interval: Duration::from_millis(250),
    }.render();

    enum ServerCommand {
        SetProject(String),
        Input(Input),
    }

    let (proj_sender, proj_receiver) = channel();

    struct State {
        extension: String,
        running: AtomicBool,
        current_proj: Mutex<String>,
        proj_sender: Mutex<Sender<ServerCommand>>,
        output: Mutex<String>,
        errors: Mutex<Vec<ErrorSummary>>,
    }
    let state = web::Data::new(State {
        extension,
        running: AtomicBool::new(true),
        current_proj: Mutex::new(EMPTY_PROJECT.into()),
        proj_sender: Mutex::new(proj_sender),
        output: Mutex::new(String::with_capacity(1024)),
        errors: Mutex::new(Vec::with_capacity(8)),
    });

    macro_rules! tee_println {
        ($state:expr => $($t:tt)*) => {{
            let content = format!($($t)*);
            if let Some(state) = $state {
                let mut output = state.output.lock().unwrap();
                output.push_str(&content);
                output.push('\n');
            }
            println!("{content}");
        }}
    }

    let weak_state = Arc::downgrade(&state);
    let config = overrides.fallback(&Config {
        request: None,
        command: Some(Rc::new(move |_, _, key, command, entity| match command {
            Command::Print { style: _, value } => {
                if let Some(value) = value { tee_println!(weak_state.upgrade() => "{entity:?} > {value:?}") }
                key.complete(Ok(()));
                CommandStatus::Handled
            }
            _ => CommandStatus::UseDefault { key, command },
        })),
    });
    let system = Rc::new(StdSystem::new_sync(nb_server, Some("native-server"), config, utc_offset));
    let mut idle_sleeper = IdleAction::new(YIELDS_BEFORE_IDLE_SLEEP, Box::new(|| thread::sleep(IDLE_SLEEP_TIME)));
    println!("public id: {}", system.get_public_id());

    #[tokio::main(flavor = "multi_thread", worker_threads = 1)]
    async fn run_http(state: web::Data<State>, port: u16) {
        #[get("/extension.js")]
        async fn get_extension(state: web::Data<State>) -> impl Responder {
            HttpResponse::Ok().content_type("text/javascript").body(state.extension.clone())
        }

        #[post("/pull")]
        async fn pull_status(state: web::Data<State>) -> impl Responder {
            let running = state.running.load(MemoryOrder::Relaxed);
            let output = mem::take(&mut *state.output.lock().unwrap());
            let errors = mem::take(&mut *state.errors.lock().unwrap());

            HttpResponse::Ok().content_type("application/json").body(serde_json::to_string(&Status { running, output, errors }).unwrap())
        }

        #[post("/project")]
        async fn set_project(state: web::Data<State>, body: web::Bytes) -> impl Responder {
            match String::from_utf8(body.to_vec()) {
                Ok(content) => {
                    state.proj_sender.lock().unwrap().send(ServerCommand::SetProject(content)).unwrap();
                    HttpResponse::Ok().content_type("text/plain").body("loaded project")
                }
                Err(_) => HttpResponse::BadRequest().content_type("text/plain").body("project was not valid utf8"),
            }
        }

        #[get("/project")]
        async fn get_project(state: web::Data<State>) -> impl Responder {
            let proj = state.current_proj.lock().unwrap().clone();
            HttpResponse::Ok().content_type("text/xml").append_header(("Content-Disposition", "attachment; filename=\"project.xml\"")).body(proj)
        }

        #[post("/input")]
        async fn send_input(state: web::Data<State>, input: web::Bytes) -> impl Responder {
            let input = match String::from_utf8(input.to_vec()) {
                Ok(input) => match input.as_str() {
                    "start" => Input::Start,
                    "stop" => Input::Stop,
                    _ => return HttpResponse::BadRequest().content_type("text/plain").body(format!("unknown input: {input:?}")),
                }
                Err(_) => return HttpResponse::BadRequest().content_type("text/plain").body("input was not valid utf8")
            };
            state.proj_sender.lock().unwrap().send(ServerCommand::Input(input)).unwrap();
            HttpResponse::Ok().content_type("text/plain").body("sent input")
        }

        #[post("/toggle-paused")]
        async fn toggle_paused(state: web::Data<State>) -> impl Responder {
            state.running.fetch_xor(true, MemoryOrder::Relaxed);
            HttpResponse::Ok().content_type("text/plain").body("toggled pause state")
        }

        HttpServer::new(move || {
            App::new()
                .wrap(Cors::permissive())
                .app_data(web::PayloadConfig::new(MAX_REQUEST_SIZE_BYTES))
                .app_data(state.clone())
                .service(get_extension)
                .service(pull_status)
                .service(set_project)
                .service(get_project)
                .service(send_input)
                .service(toggle_paused)
        })
        .workers(1)
        .bind(("localhost", port)).unwrap().run().await.unwrap();
    }
    let weak_state = Arc::downgrade(&state);
    thread::spawn(move || run_http(state, port));

    let (_, empty_role) = open_project(EMPTY_PROJECT, None).unwrap_or_else(|_| crash!(666: "default project failed to load"));
    let mut env = get_env(&empty_role, system.clone()).unwrap();

    'program: loop {
        'input: loop {
            match proj_receiver.try_recv() {
                Ok(command) => match command {
                    ServerCommand::SetProject(content) => match open_project(&content, None) {
                        Ok((proj_name, role)) => {
                            let mut state = weak_state.upgrade().unwrap();
                            tee_println!(Some(&mut state) => "\n>>> loaded project '{proj_name}'\n");
                            match get_env(&role, system.clone()) {
                                Ok(x) => {
                                    env = x;
                                    *state.current_proj.lock().unwrap() = content;
                                }
                                Err(e) => tee_println!(Some(&mut state) => "\n>>> project load error: {e:?}\n>>> keeping previous project...\n"),
                            }
                        }
                        Err(e) => match e {
                            OpenProjectError::ParseError { error } if error.location.collab_id.is_some() => {
                                let mut state = weak_state.upgrade().unwrap();
                                let cause = format!("{:?}", error.kind);
                                state.errors.lock().unwrap().push(ErrorSummary {
                                    cause: cause.clone(),
                                    entity: error.location.entity.unwrap_or_default(),
                                    globals: vec![],
                                    fields: vec![],
                                    trace: vec![TraceEntry { location: error.location.collab_id.unwrap(), locals: vec![] }], // unwrap safe because of branch guard condition
                                });
                                tee_println!(Some(&mut state) => "\n>>> project load error: {cause:?}\n>>> see red error comments...\n>>> keeping previous project...\n");
                            }
                            _ => tee_println!(weak_state.upgrade() => "\n>>> project load error: {e:?}\n>>> keeping previous project...\n"),
                        }
                    }
                    ServerCommand::Input(input) => {
                        if let Input::Start = &input {
                            if let Some(state) = weak_state.upgrade() {
                                state.running.store(true, MemoryOrder::Relaxed);
                            }
                        }
                        env.mutate(|mc, env| env.proj.borrow_mut(mc).input(mc, input));
                    }
                }
                Err(TryRecvError::Disconnected) => break 'program,
                Err(TryRecvError::Empty) => break 'input,
            }
        }
        if !weak_state.upgrade().map(|state| state.running.load(MemoryOrder::Relaxed)).unwrap_or(true) {
            idle_sleeper.trigger();
            continue;
        }

        env.mutate(|mc, env| {
            let mut proj = env.proj.borrow_mut(mc);
            for _ in 0..STEPS_PER_IO_ITER {
                let res = proj.step(mc);
                match &res {
                    ProjectStep::Error { error, proc } => if let Some(state) = weak_state.upgrade() {
                        let summary = ErrorSummary::extract(error, proc, &env.locs);

                        tee_println!(Some(&state) => "\n>>> runtime error in entity {:?}: {:?}\n>>> see red error comments...\n", summary.entity, summary.cause);

                        state.errors.lock().unwrap().push(summary);
                    }
                    ProjectStep::Pause => if let Some(state) = weak_state.upgrade() {
                        state.running.store(false, MemoryOrder::Relaxed);
                        break
                    }
                    _ => (),
                }
                idle_sleeper.consume(&res);
            }
        });
    }
}

/// Runs a CLI client using the given [`Mode`] configuration.
pub fn run<C: CustomTypes<StdSystem<C>>>(mode: Mode, config: Config<C, StdSystem<C>>, syscalls: &[SyscallMenu]) {
    let utc_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    match mode {
        Mode::Run { src, role, server } => {
            let content = read_file(&src).unwrap_or_else(|_| crash!(1: "failed to read file '{src}'"));
            let (project_name, role) = open_project(&content, role.as_deref()).unwrap_or_else(|e| crash!(2: "{e}"));

            if stdout().is_tty() {
                run_proj_tty(&project_name, server, &role, config, utc_offset);
            } else {
                run_proj_non_tty(&project_name, server, &role, config, utc_offset);
            }
        }
        Mode::Dump { src, role } => {
            let content = read_file(&src).unwrap_or_else(|_| crash!(1: "failed to read file '{src}'"));
            let (_, role) = open_project(&content, role.as_deref()).unwrap_or_else(|e| crash!(2: "{e}"));

            let (bytecode, _, _, _) = ByteCode::compile(&role).unwrap();
            println!("instructions:");
            bytecode.dump_code(&mut std::io::stdout().lock()).unwrap();
            println!("\ndata:");
            bytecode.dump_data(&mut std::io::stdout().lock()).unwrap();
            println!("\ntotal size: {}", bytecode.total_size());
        }
        Mode::Start { server, addr, port } => {
            run_server(server, addr, port, config, utc_offset, syscalls);
        }
    }
}
