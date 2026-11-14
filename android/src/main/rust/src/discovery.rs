use anyhow::Error;
use discovery::DiscoveryContext;
use jni::objects::{GlobalRef, JValue};

pub use discovery::{DiscoveryObserver, DiscoveryService};

use super::get_current_env;

pub struct DiscoveryServiceObserver(pub GlobalRef);

unsafe impl Send for DiscoveryServiceObserver {}
unsafe impl Sync for DiscoveryServiceObserver {}

impl DiscoveryServiceObserver {
    fn emit_peer_event(&self, method: &str, ctx: DiscoveryContext<'_>) {
        let mut env = get_current_env();
        if let Err(e) = env.with_local_frame(16, |env| {
            env.call_method(
                self.0.as_obj(),
                method,
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    JValue::Object(&env.new_string(ctx.local_id)?.into()),
                    JValue::Object(&env.new_string(ctx.id)?.into()),
                    JValue::Object(&env.new_string(ctx.ip.to_string())?.into()),
                ],
            )?;

            Ok::<(), Error>(())
        }) {
            log::error!("DiscoveryObserver {method} error={:?}", e);
        }
    }
}

impl DiscoveryObserver for DiscoveryServiceObserver {
    async fn online(&self, ctx: DiscoveryContext<'_>) {
        log::info!(
            "devices manager device online, id={}, ip={}",
            ctx.id,
            ctx.ip
        );
        self.emit_peer_event("onLine", ctx);
    }

    async fn offline(&self, ctx: DiscoveryContext<'_>) {
        log::info!("devices manager device offline, id={}", ctx.id);
        self.emit_peer_event("offLine", ctx);
    }

    async fn on_message(&self, ctx: DiscoveryContext<'_>, message: Vec<u8>) {
        log::info!(
            "devices manager device onmessage, id={}, message={:?}",
            ctx.id,
            std::str::from_utf8(&message)
        );

        let mut env = get_current_env();
        if let Err(e) = env.with_local_frame(16, |env| {
            env.call_method(
                self.0.as_obj(),
                "onMessage",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[B)V",
                &[
                    JValue::Object(&env.new_string(ctx.local_id)?.into()),
                    JValue::Object(&env.new_string(ctx.id)?.into()),
                    JValue::Object(&env.new_string(ctx.ip.to_string())?.into()),
                    JValue::Object(&env.byte_array_from_slice(&message)?.into()),
                ],
            )?;

            Ok::<(), Error>(())
        }) {
            log::error!("DiscoveryObserver on message error={:?}", e);
        }
    }
}
