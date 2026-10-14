//! Watches the default PipeWire audio sink and reports its volume.
//!
//! Changing the volume with `wpctl` (or anything else) does not create or remove
//! a global object, so it never shows up as a registry `global` event. Instead it
//! updates the `Props` parameter of the sink node, which we subscribe to here.
//!
//! PipeWire objects are not `Send`, so the whole client lives on a dedicated
//! thread. It fans out percentages to subscribers over channels; the UI side
//! awaits one of those channels without sharing any PipeWire state.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use pipewire as pw;
use pw::registry::{GlobalObject, RegistryWeak};
use pw::spa::param::ParamType;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::{Pod, Value, ValueArray};
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;

/// `SPA_PROP_channelVolumes`: the per-channel linear volumes inside a `Props`
/// parameter.
const SPA_PROP_CHANNEL_VOLUMES: u32 = 65544;

/// `SPA_PROP_mute`: the mute flag inside a `Props` parameter.
const SPA_PROP_MUTE: u32 = 65540;

/// State of the default sink, as reported to subscribers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Volume {
    /// Unmuted, at this percentage (`0..=100`).
    Percent(u8),
    /// Muted.
    Muted,
}

/// Subscribers that want the latest state, fanned out from the watcher thread.
type Subscribers = Arc<Mutex<Vec<async_channel::Sender<Volume>>>>;

/// The lazily-started watcher is shared by every subscriber (one per window).
static SUBSCRIBERS: OnceLock<Subscribers> = OnceLock::new();

/// Subscribes to default-sink changes (volume percentage or mute).
///
/// The PipeWire watcher is started on the first call and lives for the lifetime
/// of the process.
pub fn subscribe() -> async_channel::Receiver<Volume> {
    let (sender, receiver) = async_channel::unbounded();

    let subscribers = SUBSCRIBERS.get_or_init(|| {
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let watcher_subscribers = subscribers.clone();
        thread::Builder::new()
            .name("pipewire-volume".to_string())
            .spawn(move || {
                if let Err(err) = watch(&watcher_subscribers) {
                    eprintln!("pipewire volume watcher stopped: {err}");
                }
            })
            .ok();
        subscribers
    });

    subscribers.lock().unwrap().push(sender);
    receiver
}

/// The state the watcher builds up as globals arrive: which sink is the default
/// and the last known volume of each sink.
struct Volumes {
    subscribers: Subscribers,
    default_sink: Option<String>,
    by_name: HashMap<String, Volume>,
    last_sent: Option<Volume>,
}

impl Volumes {
    fn new(subscribers: Subscribers) -> Self {
        Self {
            subscribers,
            default_sink: None,
            by_name: HashMap::new(),
            last_sent: None,
        }
    }

    /// Publishes `volume` to every live subscriber, skipping repeats.
    fn report(&mut self, volume: Volume) {
        if self.last_sent == Some(volume) {
            return;
        }
        self.last_sent = Some(volume);

        let subscribers = self.subscribers.lock().unwrap();
        for sender in subscribers.iter() {
            // Only fails once the receiver has been dropped.
            let _ = sender.try_send(volume);
        }
    }

    /// Records the default sink and reports its volume if we already know it.
    fn set_default(&mut self, name: String) {
        self.default_sink = Some(name.clone());
        if let Some(&volume) = self.by_name.get(&name) {
            self.report(volume);
        }
    }

    /// Records a sink's state, reporting it when that sink is the default.
    fn set_state(&mut self, name: &str, volume: Volume) {
        self.by_name.insert(name.to_string(), volume);
        if self.default_sink.as_deref() == Some(name) {
            self.report(volume);
        }
    }
}

/// Proxies and listeners have to stay alive for the events to keep arriving.
type KeepAlive = Rc<RefCell<HashMap<u32, Vec<Box<dyn Any>>>>>;

/// Connects to PipeWire and reports the default sink's volume until the thread
/// is torn down at process exit.
fn watch(subscribers: &Subscribers) -> Result<(), pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let volumes = Rc::new(RefCell::new(Volumes::new(subscribers.clone())));
    let keep_alive: KeepAlive = Rc::new(RefCell::new(HashMap::new()));

    let registry_weak = registry.downgrade();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let volumes = volumes.clone();
            let keep_alive = keep_alive.clone();
            move |global| on_global(global, &registry_weak, &volumes, &keep_alive)
        })
        .global_remove({
            let keep_alive = keep_alive.clone();
            move |id| {
                keep_alive.borrow_mut().remove(&id);
            }
        })
        .register();

    main_loop.run();
    Ok(())
}

fn on_global(
    global: &GlobalObject<&DictRef>,
    registry_weak: &RegistryWeak,
    volumes: &Rc<RefCell<Volumes>>,
    keep_alive: &KeepAlive,
) {
    let Some(registry) = registry_weak.upgrade() else {
        return;
    };

    match global.type_ {
        // The `default` metadata tells us which sink `wpctl` targets; it is
        // replayed on bind, so we learn the current default immediately.
        ObjectType::Metadata => {
            if global.props.and_then(|props| props.get("metadata.name")) != Some("default") {
                return;
            }

            let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
                return;
            };
            let listener = metadata
                .add_listener_local()
                .property({
                    let volumes = volumes.clone();
                    move |_subject, key, _type, value| {
                        if key == Some("default.audio.sink")
                            && let Some(name) = value.and_then(parse_sink_name)
                        {
                            volumes.borrow_mut().set_default(name);
                        }
                        0
                    }
                })
                .register();

            keep_alive
                .borrow_mut()
                .entry(global.id)
                .or_default()
                .extend([
                    Box::new(metadata) as Box<dyn Any>,
                    Box::new(listener) as Box<dyn Any>,
                ]);
        }
        // Every hardware sink is a node. Subscribe to its `Props` and ask for the
        // current value so we have a reading before the first change.
        ObjectType::Node => {
            let props = global.props;
            if props.and_then(|props| props.get("media.class")) != Some("Audio/Sink") {
                return;
            }
            let Some(node_name) = props
                .and_then(|props| props.get("node.name"))
                .map(str::to_string)
            else {
                return;
            };

            let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                return;
            };
            node.subscribe_params(&[ParamType::Props]);
            let listener = node
                .add_listener_local()
                .param({
                    let volumes = volumes.clone();
                    move |_seq, param_type, _index, _next, param| {
                        if param_type != ParamType::Props {
                            return;
                        }
                        // Enumeration and change events end with a `None` pod.
                        if let Some(volume) = param.and_then(volume_state) {
                            volumes.borrow_mut().set_state(&node_name, volume);
                        }
                    }
                })
                .register();
            node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);

            keep_alive
                .borrow_mut()
                .entry(global.id)
                .or_default()
                .extend([
                    Box::new(node) as Box<dyn Any>,
                    Box::new(listener) as Box<dyn Any>,
                ]);
        }
        _ => {}
    }
}

/// Reads the mute flag and averaged channel volume from a `Props` pod, converting
/// the linear volume to the percentage `wpctl` displays.
fn volume_state(pod: &Pod) -> Option<Volume> {
    let (_, value) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };

    let muted = object
        .properties
        .iter()
        .find(|prop| prop.key == SPA_PROP_MUTE)
        .is_some_and(|prop| prop.value == Value::Bool(true));
    if muted {
        return Some(Volume::Muted);
    }

    let channels = object
        .properties
        .iter()
        .find(|prop| prop.key == SPA_PROP_CHANNEL_VOLUMES)?;
    let Value::ValueArray(ValueArray::Float(volumes)) = &channels.value else {
        return None;
    };
    if volumes.is_empty() {
        return None;
    }

    // PipeWire stores linear volumes; `wpctl` shows their cube root. Match it so
    // the island and `wpctl` agree.
    let average = volumes.iter().sum::<f32>() / volumes.len() as f32;
    let percent = (average.max(0.0).cbrt() * 100.0).round();
    Some(Volume::Percent(percent.clamp(0.0, 100.0) as u8))
}

/// Extracts the sink name from a metadata value like `{"name":"alsa_output..."}`.
fn parse_sink_name(value: &str) -> Option<String> {
    let rest = value.split_once("\"name\"")?.1;
    let rest = rest.split_once(':')?.1.trim_start();
    let rest = rest.strip_prefix('"')?;
    let (name, _) = rest.split_once('"')?;
    Some(name.to_string())
}
