mod callbacks;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::{
    cell::RefCell,
    error::Error,
    ops::Deref,
};

use chrono::Local;
use std::io::Write;

use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, trace};
use pulse::{
    callbacks::ListResult,
    context::{
        subscribe::{Facility, InterestMaskSet, Operation},
        Context, FlagSet, State,
    },
    mainloop::threaded::Mainloop,
    proplist::Proplist,
};

type RContext = Rc<RefCell<Context>>;
type RMainloop = Rc<RefCell<Mainloop>>;

type Sources = HashMap<u32, SourceDatum>;

#[derive(Parser, Debug)]
#[clap(author = "Sam Martin-Brown", version, about)]
/// Application configuration
struct Args {
    /// whether to be verbose
    #[arg(short = 'v')]
    verbose: bool,

    /// an optional name to greet
    #[arg()]
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct SourceDatum {
    name: String,
    mute: bool,
}
impl SourceDatum {
    fn new(name: String, mute: bool) -> Self {
        SourceDatum {
            name: name.to_string(),
            mute,
        }
    }
}

#[derive(Debug, Clone)]
struct ListenerState {
    // Use Pulseaudio's source index as key to source data (which is just name and mute-status)
    sources: Sources,
    default_source: u32,
}

impl ListenerState {
    fn new(mainloop: &RMainloop, context: &RContext) -> Result<Self, Box<dyn Error>> {
        let sources = get_sources(context, mainloop)?;
        let default_source = get_default_source_index(mainloop, context, &sources)?;
        Ok(Self {
            sources,
            default_source,
        })
    }

    fn default_source<'a>(&'a self) -> Option<&'a SourceDatum> {
        self.sources.get(&self.default_source)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logs();

    let mainloop = Rc::new(RefCell::new(Mainloop::new().ok_or("mainloop new failed")?));

    let proplist = Proplist::new().ok_or("proplist failed")?;
    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(mainloop.borrow_mut().deref(), "source-listener", &proplist)
            .ok_or("context::new_with_proplist failed")?,
    ));

    info!("Connecting to daemon");
    connect_to_server(&context, &mainloop)?;

    debug!("We should be connected at this point..!");

    let state = ListenerState::new(&mainloop, &context)?;
    subscribe_source_mute(mainloop, context, state)
}

enum SrcListState {
    // InProg,
    Item(u32, SourceDatum),
    Done,
    Err(String),
}

fn get_sources(context: &RContext, mainloop: &RMainloop) -> Result<Sources, Box<dyn Error>> {
    // Lock mainloop to block pulseaudio from calling things during setup
    mainloop.borrow_mut().lock();

    let introspector = context.borrow_mut().introspect();

    let (src_tx, src_rx) = mpsc::channel();
    introspector.get_source_info_list(move |src| match src {
        ListResult::Error => {
            let msg = "Failed to retrieve ListResult".into();
            error!("{}", msg);
            src_tx.send(SrcListState::Err(msg)).unwrap();
        }
        ListResult::End => {
            src_tx.send(SrcListState::Done).unwrap();
        }
        ListResult::Item(item) => {
            let source_name = match &item.name {
                None => "unknown".to_string(),
                Some(name) => name.to_string(),
            };

            src_tx.send(
                SrcListState::Item(
                    item.index,
                    SourceDatum::new(source_name, item.mute)
                )
            ).unwrap();
        }
    });

    let mut sources = HashMap::new();

    // Unlock mainloop to let pulseaudio call the above callback.
    mainloop.borrow_mut().unlock();
    loop {
        let item = src_rx.recv()?;

        match item {
            SrcListState::Item(index, source) => {
                sources.insert(index, source);
            }
            SrcListState::Done => {
                trace!("Retrieved source info");
                return Ok(sources);
            }
            SrcListState::Err(err) => {
                error!("Caught error waiting: {}", err);
                return Err(err.to_owned().into());
            }
        }
    }
}

#[derive(Debug, Clone)]
enum DefaultSourceState {
    NoDefault,
    Default(String),
}

fn find_default_source_name(
    context: &RContext,
    mainloop: &RMainloop,
) -> Result<String, Box<dyn Error>> {
    // Block pulseaudio from inboking callbacks
    mainloop.borrow_mut().lock();

    let introspector = context.borrow_mut().introspect();
    let (src_tx, src_rx) = mpsc::channel();

    {
        introspector.get_server_info(move |server_info| {
            trace!("Server info: {:?}", server_info);
            match &server_info.default_source_name {
                None => {
                    info!("no default source");
                    src_tx.send(DefaultSourceState::NoDefault).unwrap()
                }
                Some(value) => {
                    info!("Default source: '{:?}'", value);
                    src_tx
                        .send(DefaultSourceState::Default(value.to_string()))
                        .unwrap();
                }
            };
        });
    }

    // Allow pulseaudio to process callbacks again
    mainloop.borrow_mut().unlock();
    loop {
        trace!("grabbing default source value");
        let default_source = src_rx.recv()?;
        trace!("Grabbed default source");
        match default_source {
            DefaultSourceState::NoDefault => {
                return Ok("No default source".to_owned());
            }
            DefaultSourceState::Default(name) => {
                trace!("Returning from get_sources");
                return Ok(name.to_owned());
            }
        };
    }
}

fn get_default_source_index(
    mainloop: &RMainloop,
    context: &RContext,
    sources: &Sources,
) -> Result<u32, Box<dyn Error>> {
    let default_source_name = find_default_source_name(context, mainloop)?;

    for (index, source) in sources {
        if source.name == default_source_name {
            debug!("Default source is: '{}', index: {}", source.name, index);
            return Ok(*index);
        }
    }

    error!("failed to set default source");
    Err("failed to set default source".into())
}

fn setup_logs() {
    let args = Args::parse();
    let log_env = if args.verbose {
        Env::default().default_filter_or("debug")
    } else {
        Env::default().default_filter_or("info")
    };
    env_logger::Builder::from_env(log_env)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}:{}] ({}): {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S%.6f%z"),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.level(),
                record.args(),
            )
        })
        .init();
}

fn subscribe_source_mute(
    mainloop: RMainloop,
    context: RContext,
    mut state: ListenerState,
) -> Result<(), Box<dyn Error>> {
    // Sources toggle their mute state, default source changes Server state
    let source_mask = InterestMaskSet::SOURCE | InterestMaskSet::SERVER;

    trace!("Configuring context subscriber");

    // Block pulseaudio from invoking callbacks
    mainloop.borrow_mut().lock();

    let (tx, rx) = mpsc::channel();
    // tell pulseaudio to notify us about Source & Server changes
    {
        // set callback that reacts to subscription changes
        context.borrow_mut().set_subscribe_callback(Some(Box::new(
            move |facility: Option<Facility>, operation: Option<Operation>, idx| {
                let facility = facility.unwrap();
                let operation = operation.unwrap();
                debug!(
                    "Subcribe callback: {:?}, {:?}, {:?}",
                    facility, operation, idx
                );

                match facility {
                    Facility::Source => {
                        match operation {
                            Operation::Changed => {
                                // if state.default_source == id {
                                // trace!("Default source changed config");
                                // let old_mute_state = state.default_source().unwrap().mute;

                                // tell callback that mainloop should update sources (can't do that here since
                                // we're already inside a callback).
                                // trace!("Source {} Changed", idx);
                                tx.send(Facility::Source).unwrap();
                            }
                            Operation::New => {
                                debug!("New source added with index {}", idx);
                            }
                            Operation::Removed => {
                                debug!("Source with index {} removed", idx);
                            }
                        }
                    }
                    Facility::Server => {
                        info!("Server change event");
                        let _ = tx.send(Facility::Server);
                    }
                    _ => debug!("Unrelated event: {:?}", facility),
                }
            },
        )));
    }

    context.borrow_mut().subscribe(source_mask, |sub_success| {
        debug!(
            "Subscribing to source changes {}",
            match sub_success {
                true => "succeeded",
                false => "failed",
            }
        );
    });

    // TODO: We should also bind to shutdown signal for clean teardown here...
    trace!("Starting subscribe mainloop");

    // Allow pulseaudio to process callbacks again
    mainloop.borrow_mut().unlock();
    loop {
        // When we receive data via channel here, it means, we should update sources, and then
        // print if the mute state of the default source, changed.

        let old_default_mute = {
            match state.default_source() {
                Some(src) => Some(src.mute),
                None => None,
            }
        };
        trace!("current source mute state: {:?}", &old_default_mute);

        let event_type = rx.recv()?;
        match event_type {
            Facility::Server => {
                let _ = handle_server_change(&mut state, &mainloop, &context);
                // Always check source changes, to ensure the new default's mute state is compared
                // against prior mute state.
                state.sources = get_sources(&context, &mainloop).unwrap();
            }
            Facility::Source => {
                state.sources = get_sources(&context, &mainloop).unwrap();
            }
            _ => {
                panic!("impossible state");
            }
        }

        if let Some(new_src) = state.default_source() {
            if Some(new_src.mute) != old_default_mute {
                println!(
                    "{}",
                    match new_src.mute {
                        true => "MUTED",
                        false => "UNMUTED",
                    }
                );
            }
        } else {
            println!("No default source");
        }
    }
}

fn handle_server_change(
    state: &mut ListenerState,
    mainloop: &RMainloop,
    context: &RContext,
) -> Result<(), Box<dyn Error>> {
    // Check if default source changed and update state
    debug!("Updating default source after server config change");
    // TODO: Do we need to check if the source map needs updating...?
    // Ideally - we collect sources once on start, then use the source add/remove subscriptions to
    // keep updated...
    state.default_source = get_default_source_index(mainloop, context, &state.sources)?;

    debug!(
        "Default source is now: {}",
        state.default_source().unwrap().name
    );

    Ok(())
}


fn connect_to_server(context: &RContext, mainloop: &RMainloop) -> Result<(), Box<dyn Error>> {
    trace!("Calling context.connect");
    mainloop.borrow_mut().lock();

    let (tx, rx) = mpsc::channel();
    {
        // Context state boxed-callback setup
        trace!("Registering context state callback");
        context
            .borrow_mut()
            .set_state_callback(Some(Box::new(move || {
                trace!("context state changed");
                tx.send(Some(())).unwrap();
            })));
    }

    context
        .borrow_mut()
        .connect(None, FlagSet::NOAUTOSPAWN, None)?;

    mainloop.borrow_mut().unlock();
    mainloop.borrow_mut().start()?;

    loop {
        trace!("Waiting for context state-change callback");
        let _ = rx.recv(); // Wait for signal from callback.
        trace!("received");

        let state = context.borrow().get_state();
        match state {
            State::Unconnected | State::Connecting | State::Authorizing | State::SettingName => {
                debug!("Context state: {:?}", state);
                continue; // Use channel for synchronisation
            }
            State::Ready => {
                debug!("Context state: {:?}", state);
                break;
            }
            State::Failed => {
                debug!("Context state: {:?}", state);
                return Err("Context connect failed".into());
            }
            State::Terminated => {
                debug!("Context state: {:?}", state);
                return Err("Context terminated".into());
            }
        }
    }
    // Once connected, we don't care anymore...
    context.borrow_mut().set_state_callback(None);

    Ok(())
}
