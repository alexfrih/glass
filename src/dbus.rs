//! org.freedesktop.Notifications DBus server.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zbus::{connection, interface, zvariant::Value, Connection};

use crate::wayland::{Event, Reply};

pub async fn serve(tx: UnboundedSender<Event>, mut reply_rx: UnboundedReceiver<Reply>) -> Result<()> {
    let server = Notifications {
        next_id: Arc::new(AtomicU32::new(1)),
        tx,
    };
    let conn = connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", server)?
        .build()
        .await?;
    tracing::info!("dbus server bound to org.freedesktop.Notifications");

    // Spawn signal emitter task — reads replies from wayland thread.
    tokio::spawn(signal_emitter(conn.clone(), reply_rx));

    std::future::pending::<()>().await;
    Ok(())
}

async fn signal_emitter(conn: Connection, mut rx: UnboundedReceiver<Reply>) {
    while let Some(reply) = rx.recv().await {
        let iface = conn
            .object_server()
            .interface::<_, Notifications>("/org/freedesktop/Notifications")
            .await;
        let Ok(iface_ref) = iface else {
            tracing::error!("failed to get Notifications iface for signal");
            continue;
        };
        let ctx = iface_ref.signal_emitter();
        match reply {
            Reply::Closed { id, reason } => {
                let _ = Notifications::notification_closed(&ctx, id, reason).await;
                tracing::debug!(id, reason, "emitted NotificationClosed");
            }
            Reply::ActionInvoked { id, action } => {
                let _ = Notifications::action_invoked(&ctx, id, &action).await;
                tracing::debug!(id, action = %action, "emitted ActionInvoked");
            }
        }
    }
}

pub struct Notifications {
    next_id: Arc<AtomicU32>,
    tx: UnboundedSender<Event>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        _hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            replaces_id
        };

        let timeout_ms = if expire_timeout < 0 {
            5000
        } else if expire_timeout == 0 {
            0
        } else {
            expire_timeout as u32
        };

        tracing::info!(id, app = %app_name, summary = %summary, "notify");

        let _ = self.tx.send(Event::Notify {
            id,
            app_name,
            app_icon,
            summary,
            body,
            actions,
            timeout_ms,
        });

        id
    }

    async fn close_notification(&self, id: u32) {
        let _ = self.tx.send(Event::Close { id });
    }

    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["body", "actions", "persistence", "icon-static"]
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("glass", "glass-notify", env!("CARGO_PKG_VERSION"), "1.2")
    }

    #[zbus(signal)]
    async fn notification_closed(
        ctx: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        ctx: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}
