//! Redis pub/sub transport: topic naming, wire encoding, and the TrafficClient handle.

use std::time::Duration;

use log::{debug, info, warn};
use redis::AsyncCommands;
use tokio::sync::mpsc;

use crate::data::{ClientData, DataError, RawData};
use crate::settings::{Settings, authed_url};
use crate::transport::{DataFile, TransportError};

// --- Topic helpers -----------------------------------------------------------

/// Data topic for a project: `dt:<project>`.
pub fn project_topic(project: &str) -> String {
    format!("dt:{project}")
}

/// Global presence notification topic.
pub const PRESENCE_TOPIC: &str = "dt:__presence__";

/// Redis SET key tracking active clients for a project.
pub fn presence_set_key(project: &str) -> String {
    format!("dt:__presence__:{project}")
}

// --- Wire format (encrypted envelope) ----------------------------------------
// Layout: [sender_len u32le][sender_utf8][ClientData blob]
// Sender stays in the clear for cheap self-filtering; name/note/data are inside the AES-256-GCM blob.

/// Build the encrypted wire payload: readable sender prefix plus an AES-256-GCM envelope.
pub fn encode_message(sender: &str, file: &DataFile, key: &[u8; 32]) -> Result<Vec<u8>, DataError> {
    let raw = RawData::new(file.name.clone(), file.note.clone(), file.data.clone());
    let blob = ClientData::encrypt(&raw, key)?.encode();
    let mut buf = Vec::with_capacity(4 + sender.len() + blob.len());
    write_len_str(&mut buf, sender);
    buf.extend_from_slice(&blob);
    Ok(buf)
}

/// Parse an encrypted wire payload into (sender_name, decrypted DataFile).
pub fn decode_message(data: &[u8], key: &[u8; 32]) -> Result<(String, DataFile), DataError> {
    let mut pos = 0;
    let sender = read_len_str(data, &mut pos).map_err(
        |e| DataError::Malformed(e.to_string())
    )?;
    if pos > data.len() {
        return Err(DataError::Malformed("truncated envelope".into()));
    }
    let client_data = ClientData::decode(&data[pos..])?;
    let raw = client_data.decrypt(key)?;
    Ok((sender, DataFile { name: raw.name, note: raw.note, data: raw.data }))
}

/// Read only the sender prefix from a wire payload (cheap, for filtering own messages).
pub fn envelope_sender(data: &[u8]) -> Result<String, TransportError> {
    let mut pos = 0;
    read_len_str(data, &mut pos)
}

fn write_len_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_len_str(data: &[u8], pos: &mut usize) -> Result<String, TransportError> {
    if *pos + 4 > data.len() {
        return Err(TransportError::TooShort);
    }
    let len = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]) as usize;
    *pos += 4;
    if *pos + len > data.len() {
        return Err(TransportError::TooShort);
    }
    let s = String::from_utf8(data[*pos..*pos + len].to_vec()).map_err(
        |_| TransportError::BadUtf8
    )?;
    *pos += len;
    Ok(s)
}

// --- TrafficClient -----------------------------------------------------------

/// Public handle for exchanging DataFiles over Redis pub/sub.
/// Watches a list of projects; outbox messages carry the target project per message: `(project, DataFile)`.
pub struct TrafficClient {
    settings: Settings,
    watch_projects: Vec<String>,
    outbox_tx: mpsc::Sender<(String, DataFile)>,
    inbox_rx: mpsc::Receiver<(String, DataFile)>,
}

impl TrafficClient {
    /// Connect to Redis, register presence, subscribe to topics, and spawn outbox/inbox background tasks.
    pub async fn new(
        settings: Settings,
        watch_projects: Vec<String>,
    ) -> Result<Self, String> {
        let url = authed_url(&settings.redis_url, settings.password.as_deref());
        let client = redis::Client::open(url.as_str()).map_err(
            |e| format!("failed to create Redis client: {e}")
        )?;

        // Multiplexed connection for publishing and SET operations.
        let mut mux_conn = client.get_multiplexed_async_connection().await.map_err(
            |e| format!("failed to get multiplexed connection: {e}")
        )?;

        // Register presence: SADD our name into each watched project's set.
        for project in &watch_projects {
            let key = presence_set_key(project);
            let ok: bool = mux_conn.sadd(&key, &settings.client_name).await.map_err(
                |e| format!("SADD failed for {key}: {e}")
            )?;
            if ok {
                debug!(
                    "registered presence '{}' in project '{}'",
                    settings.client_name, project
                );
            }
        }

        // Pub/sub connection for receiving.
        let mut pubsub_conn = client.get_async_pubsub().await.map_err(
            |e| format!("failed to get pub/sub connection: {e}")
        )?;

        for project in &watch_projects {
            let topic = project_topic(project);
            pubsub_conn.subscribe(&topic).await.map_err(
                |e| format!("SUBSCRIBE failed for {topic}: {e}")
            )?;
            info!("subscribed to {}", topic);
        }

        // Subscribe to presence topic for awareness.
        pubsub_conn.subscribe(PRESENCE_TOPIC).await.map_err(
            |e| format!("SUBSCRIBE failed for {PRESENCE_TOPIC}: {e}")
        )?;

        let (outbox_tx, mut outbox_rx) = mpsc::channel::<(String, DataFile)>(256);
        let (inbox_tx, inbox_rx) = mpsc::channel::<(String, DataFile)>(256);

        // Background task: read from outbox, encrypt + encode, publish to the per-message project.
        {
            let sender_name = settings.client_name.clone();
            let key = settings.key;
            let mut pub_conn = mux_conn.clone();
            tokio::spawn(async move {
                while let Some((project, file)) = outbox_rx.recv().await {
                    let target_topic = project_topic(&project);
                    let payload = match encode_message(&sender_name, &file, &key) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("encode '{}' failed: {e}", file.name);
                            continue;
                        }
                    };
                    match pub_conn.publish::<&str, Vec<u8>, ()>(&target_topic, payload).await {
                        Ok(_) => {
                            debug!(
                                "published '{}' ({} bytes) to {}",
                                file.name,
                                file.data.len(),
                                target_topic
                            );
                        }
                        Err(e) => {
                            warn!("publish to {} failed: {e}", target_topic);
                        }
                    }
                }
            });
        }

        // Background task: read pub/sub messages, filter own, decrypt, push to inbox.
        {
            let my_name = settings.client_name.clone();
            let key = settings.key;
            tokio::spawn(async move {
                let mut stream = pubsub_conn.on_message();
                use futures_util::StreamExt;
                while let Some(msg) = stream.next().await {
                    let channel = msg.get_channel_name().to_string();
                    if channel == PRESENCE_TOPIC {
                        continue; // skip presence notifications
                    }
                    let payload: Vec<u8> = match msg.get_payload() {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("bad payload on {}: {e}", channel);
                            continue;
                        }
                    };
                    // Filter out our own messages before decrypting (cheap).
                    let sender = match envelope_sender(&payload) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("bad envelope on {}: {e}", channel);
                            continue;
                        }
                    };
                    if sender == my_name {
                        debug!("skipping own message from {}", sender);
                        continue;
                    }
                    let (_, file) = match decode_message(&payload, &key) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("decode/decrypt error on {}: {e}", channel);
                            continue;
                        }
                    };
                    // Channel name is `dt:<project>` — the project travels with the file.
                    let project = channel.strip_prefix("dt:").unwrap_or(&channel).to_string();
                    info!(
                        "received '{}' ({} bytes) from {} on {}",
                        file.name,
                        file.data.len(),
                        sender,
                        channel
                    );
                    if inbox_tx.send((project, file)).await.is_err() {
                        break; // receiver dropped
                    }
                }
            });
        }

        // Background task: periodic presence refresh (SMEMBERS + log).
        {
            let projects = watch_projects.clone();
            let my_name = settings.client_name.clone();
            let mut pres_conn = mux_conn.clone();
            tokio::spawn(async move {
                let interval = Duration::from_secs(30);
                loop {
                    tokio::time::sleep(interval).await;
                    for project in &projects {
                        let key = presence_set_key(project);
                        match pres_conn.smembers::<_, Vec<String>>(&key).await {
                            Ok(members) => {
                                debug!(
                                    "active clients in '{}': {} (me: {})",
                                    project,
                                    members.len(),
                                    my_name
                                );
                            }
                            Err(e) => {
                                warn!("SMEMBERS {key} failed: {e}");
                            }
                        }
                    }
                }
            });
        }

        Ok(TrafficClient {
            settings,
            watch_projects,
            outbox_tx,
            inbox_rx,
        })
    }

    /// Outbox sender clone; send `(project, DataFile)` pairs, each goes to its own project topic.
    pub fn outbox(&self) -> mpsc::Sender<(String, DataFile)> {
        self.outbox_tx.clone()
    }

    /// Get mutable access to the inbox receiver for reading incoming `(project, DataFile)` pairs.
    pub fn inbox(&mut self) -> &mut mpsc::Receiver<(String, DataFile)> {
        &mut self.inbox_rx
    }

    /// The resolved settings this client was created with.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Best-effort presence cleanup: SREM our name from all watched projects.
    pub async fn shutdown(&self) {
        let url = authed_url(&self.settings.redis_url, self.settings.password.as_deref());
        let client = match redis::Client::open(url.as_str()) {
            Ok(c) => c,
            Err(_) => return,
        };
        if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
            for project in &self.watch_projects {
                let key = presence_set_key(project);
                let _: i32 = conn.srem(&key, &self.settings.client_name).await.unwrap_or(0);
            }
            info!("presence cleanup done for '{}'", self.settings.client_name);
        }
    }
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn topic_helpers() {
        assert_eq!(project_topic("alpha"), "dt:alpha");
        assert_eq!(PRESENCE_TOPIC, "dt:__presence__");
        assert_eq!(presence_set_key("beta"), "dt:__presence__:beta");
    }

    const KEY_A: [u8; 32] = [0xa5; 32];
    const KEY_B: [u8; 32] = [0x1b; 32];

    #[test]
    fn wire_roundtrip_encrypted() {
        let file = DataFile {
            name: "report.pdf".to_string(),
            note: "Q3 results".to_string(),
            data: Bytes::from_static(b"file-content-here"),
        };
        let encoded = encode_message("client-abc", &file, &KEY_A).unwrap();
        // The payload must not contain the plaintext name/note/data in the clear.
        assert!(!encoded.windows(12).any(|w| w == b"file-content"));
        let (sender, decoded) = decode_message(&encoded, &KEY_A).unwrap();
        assert_eq!(sender, "client-abc");
        assert_eq!(decoded.name, "report.pdf");
        assert_eq!(decoded.note, "Q3 results");
        assert_eq!(decoded.data, file.data);
    }

    #[test]
    fn wire_roundtrip_empty_fields() {
        let file = DataFile {
            name: String::new(),
            note: String::new(),
            data: Bytes::new(),
        };
        let encoded = encode_message("x", &file, &KEY_A).unwrap();
        let (sender, decoded) = decode_message(&encoded, &KEY_A).unwrap();
        assert_eq!(sender, "x");
        assert_eq!(decoded.name, "");
        assert_eq!(decoded.note, "");
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn wire_roundtrip_unicode() {
        let file = DataFile {
            name: "отчёт.pdf".to_string(),
            note: "примечание".to_string(),
            data: Bytes::from_static(b"\x00\x01\x02"),
        };
        let encoded = encode_message("клиент-1", &file, &KEY_A).unwrap();
        let (sender, decoded) = decode_message(&encoded, &KEY_A).unwrap();
        assert_eq!(sender, "клиент-1");
        assert_eq!(decoded.name, "отчёт.pdf");
        assert_eq!(decoded.note, "примечание");
    }

    #[test]
    fn wire_wrong_key_is_bad_payload() {
        let file = DataFile {
            name: "secret.txt".into(),
            note: String::new(),
            data: Bytes::from_static(b"payload"),
        };
        let encoded = encode_message("client-abc", &file, &KEY_A).unwrap();
        assert_eq!(decode_message(&encoded, &KEY_B).unwrap_err(), DataError::BadPayload);
    }

    #[test]
    fn wire_decode_too_short() {
        let data = [0u8; 2]; // less than one u32 length prefix
        assert!(decode_message(&data, &KEY_A).is_err());
    }

    #[test]
    fn wire_decode_truncated_sender() {
        // sender_len says 10 but only 3 bytes follow.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(10u32).to_le_bytes());
        buf.extend_from_slice(b"abc");
        assert!(decode_message(&buf, &KEY_A).is_err());
    }

    #[test]
    fn filter_own_by_sender() {
        // The sender prefix is readable in the clear so own messages can be filtered
        // without decrypting.
        let file = DataFile {
            name: "a.txt".into(),
            note: String::new(),
            data: Bytes::from_static(b"hello"),
        };
        let encoded_mine = encode_message("me", &file, &KEY_A).unwrap();
        assert_eq!(envelope_sender(&encoded_mine).unwrap(), "me"); // would be filtered

        let encoded_other = encode_message("other", &file, &KEY_A).unwrap();
        assert_ne!(envelope_sender(&encoded_other).unwrap(), "me"); // passes through
    }
}
