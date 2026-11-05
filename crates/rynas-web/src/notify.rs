//! rynas-web's side of notifications: a [`Notifier`] that forwards events to rynasd (the
//! engine's owner) and the hardware watch (fans, temperatures, provider freshness).
//!
//! Events go through a bounded queue to one worker that posts them to rynasd's internal
//! `POST /v1/notifications/events`; a full queue or an unreachable rynasd drops them with a
//! log line (emitting never blocks a request).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::Method;
use rynas_api_types::MetricsSample;
use rynas_api_types::notify::{NewEvent, NotifyAccepted};
use rynas_notify::Notifier;
use rynas_notify::checks::{HardwareWatch, ThermalReading};
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::rynasd_client::RynasdClient;

/// Events waiting to be posted.
const QUEUE: usize = 64;
/// Keys remembered by [`WebNotifier::emit_once`] (M1); past the cap the memory is cleared.
const MAX_RECENT: usize = 1000;
/// How often the hardware watch looks at the provider and the temperatures.
pub const HW_CHECK: Duration = Duration::from_secs(30);

pub struct WebNotifier {
    client: Arc<RynasdClient>,
    tx: mpsc::Sender<NewEvent>,
    rx: Mutex<Option<mpsc::Receiver<NewEvent>>>,
    recent: Mutex<HashMap<String, Instant>>,
}

impl WebNotifier {
    pub fn new(client: Arc<RynasdClient>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE);
        Arc::new(Self {
            client,
            tx,
            rx: Mutex::new(Some(rx)),
            recent: Mutex::new(HashMap::new()),
        })
    }

    /// Starts the worker on first use (needs a runtime).
    fn start(&self) {
        let mut slot = self.rx.lock().unwrap();
        if slot.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut rx = slot.take().expect("checked");
        let client = self.client.clone();
        handle.spawn(async move {
            while let Some(ev) = rx.recv().await {
                let r: Result<NotifyAccepted, _> = client
                    .call(Method::POST, "/v1/notifications/events", None, Some(&ev))
                    .await;
                if let Err(e) = r {
                    tracing::warn!("notification {:?} not delivered to rynasd: {e:?}", ev.title);
                }
            }
        });
    }

    /// Emits `ev` unless an event with the same key went out within `window` (repeated
    /// triggers such as every refused login of a locked account).
    pub fn emit_once(&self, ev: NewEvent, window: Duration) {
        let key = ev.key.clone().unwrap_or_else(|| ev.title.clone());
        {
            let mut m = self.recent.lock().unwrap();
            let now = Instant::now();
            if m.get(&key).is_some_and(|t| now.duration_since(*t) < window) {
                return;
            }
            if m.len() >= MAX_RECENT {
                m.retain(|_, t| now.duration_since(*t) < window);
                if m.len() >= MAX_RECENT {
                    m.clear();
                }
            }
            m.insert(key, now);
        }
        self.emit(ev);
    }
}

impl Notifier for WebNotifier {
    fn emit(&self, event: NewEvent) {
        self.start();
        if self.tx.try_send(event).is_err() {
            tracing::warn!("notification queue full: event dropped");
        }
    }
}

/// Temperatures next to the Synology controller's shutdown thresholds (DSM tables of the
/// model, the same in both fan modes); empty for other providers.
pub fn thermal_readings(
    hw: Option<&rynas_api_types::HardwareStatus>,
    temps: &[rynas_api_types::Temperature],
    product: &str,
) -> Vec<ThermalReading> {
    use rynas_api_types::synology::SensorClass;
    use rynas_hw_synology::model::shutdown_c;
    let Some(hw) = hw.filter(|h| h.provider.as_deref() == Some("synology")) else {
        return Vec::new();
    };
    let mut out: Vec<ThermalReading> = Vec::new();
    let mut push = |id: &str, label: &str, c: f32, class| {
        if !out.iter().any(|r| r.id == id) {
            out.push(ThermalReading {
                id: id.into(),
                label: label.into(),
                celsius: c,
                shutdown_c: shutdown_c(product, class) as f32,
            });
        }
    };
    for t in temps {
        let Some(c) = t.celsius else { continue };
        let class = if t.id == "cpu" {
            SensorClass::Cpu
        } else if t.id.starts_with("nvme") {
            SensorClass::Nvme
        } else if t.id.starts_with("bay") {
            SensorClass::Disk
        } else {
            continue;
        };
        push(&t.id, &t.label, c, class);
    }
    // asleep disks keep their last value in synohwd's status
    for b in &hw.bays {
        if let Some(c) = b.celsius {
            push(
                &format!("bay{}", b.number),
                &format!("Bay {}", b.number),
                c,
                SensorClass::Disk,
            );
        }
    }
    out
}

/// Fans, temperatures and provider freshness every [`HW_CHECK`], emitted on transitions.
pub fn spawn_hardware_watch(st: AppState) {
    tokio::spawn(async move {
        let product = rynas_hw::dmi(std::path::Path::new("/")).1;
        let mut watch = HardwareWatch::default();
        let mut tick = tokio::time::interval(HW_CHECK);
        loop {
            tick.tick().await;
            let (hw, store) = (st.hw.clone(), st.store.clone());
            let read = tokio::task::spawn_blocking(move || {
                let status = hw.status().ok().flatten();
                let temps = store
                    .latest_json()
                    .and_then(|j| serde_json::from_str::<MetricsSample>(&j).ok())
                    .map(|s| s.temps)
                    .unwrap_or_default();
                (status, temps)
            })
            .await;
            let Ok((status, temps)) = read else { continue };
            let thermal = thermal_readings(status.as_ref(), &temps, &product);
            for ev in watch
                .check(status.as_ref(), &temps)
                .into_iter()
                .chain(watch.check_thermal(&thermal))
            {
                st.notifier.emit(ev);
            }
        }
    });
}
