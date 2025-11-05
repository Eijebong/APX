use rocket_prometheus::prometheus::{IntCounterVec, opts};
use std::sync::OnceLock;

static MESSAGE_COUNTER: OnceLock<IntCounterVec> = OnceLock::new();

pub fn init_metrics() -> IntCounterVec {
    let counter = IntCounterVec::new(
        opts!("apx_messages_total", "Total number of messages processed"),
        &["room_id", "slot", "message_type", "direction"],
    )
    .expect("Failed to create message counter");

    MESSAGE_COUNTER.get_or_init(|| counter.clone());
    counter
}

pub fn record_message(room_id: &str, slot: u32, message_type: &str, direction: &str) {
    if let Some(counter) = MESSAGE_COUNTER.get() {
        counter
            .with_label_values(&[room_id, &slot.to_string(), message_type, direction])
            .inc();
    }
}
