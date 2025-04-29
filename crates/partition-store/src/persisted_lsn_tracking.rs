use std::collections::HashMap;
use std::ffi::{CStr, CString};

use bytes::Bytes;
use rocksdb::event_listener::{EventListener, FlushJobInfo};
use rocksdb::table_properties::{
    EntryType, TablePropertiesCollector, TablePropertiesCollectorFactory,
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::fsm_table::{PartitionStateMachineKey, SequenceNumber, fsm_variable};
use crate::keys::{KeyKind, TableKey};
use crate::protobuf_types::PartitionStoreProtobufValue;
use restate_types::{identifiers::PartitionId, logs::Lsn};

pub(crate) struct LatestAppliedLsnCollector {
    applied_lsns: HashMap<PartitionId, Lsn>,
}

impl LatestAppliedLsnCollector {
    fn new() -> Self {
        LatestAppliedLsnCollector {
            applied_lsns: Default::default(),
        }
    }
}

impl TablePropertiesCollector for LatestAppliedLsnCollector {
    fn add_user_key(
        &mut self,
        key: &[u8],
        value: &[u8],
        entry_type: rocksdb::table_properties::EntryType,
        _seq: u64,
        _file_size: u64,
    ) -> bool {
        if matches!(entry_type, EntryType::EntryPut) {
            if let Some((partition_id, lsn)) = extract_partition_applied_lsn(key, value) {
                self.applied_lsns
                    .entry(partition_id)
                    .and_modify(|existing| {
                        if lsn > *existing {
                            *existing = lsn;
                        }
                    })
                    .or_insert(lsn);
            }
        }
        true
    }

    fn finish(&mut self, properties: &mut HashMap<CString, CString>) -> bool {
        for (partition_id, lsn) in &self.applied_lsns {
            let property_name = format!("restate.applied_lsn.{}", partition_id);
            properties.insert(
                CString::new(property_name).unwrap(),
                CString::new(lsn.as_u64().to_string()).unwrap(),
            );
        }
        true
    }

    fn get_readable_properties(&self) -> HashMap<CString, CString> {
        let mut properties = HashMap::new();
        for (partition_id, lsn) in &self.applied_lsns {
            let property_name = format!("restate.applied_lsn.{}", partition_id);
            properties.insert(
                CString::new(property_name).unwrap(),
                CString::new(lsn.as_u64().to_string()).unwrap(),
            );
        }
        properties
    }

    fn name(&self) -> &CStr {
        c"LsnTrackingCollector"
    }
}

pub(crate) struct LatestAppliedLsnCollectorFactory {}

impl TablePropertiesCollectorFactory for LatestAppliedLsnCollectorFactory {
    type Collector = LatestAppliedLsnCollector;

    fn create(
        &mut self,
        _context: rocksdb::table_properties::TablePropertiesCollectorContext,
    ) -> LatestAppliedLsnCollector {
        LatestAppliedLsnCollector::new()
    }

    fn name(&self) -> &CStr {
        c"AppliedLsnTrackingCollectorFactory"
    }
}

pub struct PersistedLsnEventListener {
    pub(crate) persisted_lsn_tx: mpsc::Sender<(PartitionId, Lsn)>,
}

impl EventListener for PersistedLsnEventListener {
    fn on_flush_completed(&self, flush_job_info: FlushJobInfo) {
        if let Some(id_str) = flush_job_info.cf_name.strip_prefix("data-") {
            let Ok(id) = id_str.parse::<u16>() else {
                warn!(
                    "Failed to parse partition id from cf_name: {}",
                    flush_job_info.cf_name
                );
                return;
            };

            let partition_id = PartitionId::from(id);
            let key = format!("restate.applied_lsn.{}", id);
            if let Some(applied_lsn) = flush_job_info.get_user_collected_property(&key) {
                match applied_lsn.to_str().expect("valid string").parse::<u64>() {
                    Ok(persisted_lsn) => {
                        // ignore shutdown errors
                        let _ = self
                            .persisted_lsn_tx
                            .blocking_send((partition_id, persisted_lsn.into()));
                    }
                    Err(err) => warn!(
                        key,
                        "Failed to parse applied LSN from table property: {}", err
                    ),
                };
            }
        }
    }
}

/// Given a raw key-value pair, extract the partition id and its applied LSN, if the key is an Applied LSN FSM variable
fn extract_partition_applied_lsn(key: &[u8], value: &[u8]) -> Option<(PartitionId, Lsn)> {
    if !key.starts_with(KeyKind::Fsm.as_bytes()) {
        return None;
    }

    let mut key_buf = Bytes::copy_from_slice(key);
    if let Ok(fsm_key) = PartitionStateMachineKey::deserialize_from(&mut key_buf) {
        if fsm_key.state_id == Some(fsm_variable::APPLIED_LSN) {
            if let Some(padded_partition_id) = fsm_key.partition_id {
                let partition_id = PartitionId::from(padded_partition_id);
                let mut value_buf = Bytes::copy_from_slice(value);
                if let Some(applied_lsn) = get_applied_lsn_from_raw_value(&mut value_buf) {
                    return Some((partition_id, applied_lsn));
                }
            }
        }
    }
    None
}

fn get_applied_lsn_from_raw_value<B: bytes::Buf>(value: &mut B) -> Option<Lsn> {
    match SequenceNumber::decode(value) {
        Ok(seq_number) => Some(Lsn::from(u64::from(seq_number))),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use restate_storage_api::StorageError;
    use restate_types::storage::StorageCodec;

    use crate::protobuf_types::ProtobufStorageWrapper;

    use super::*;

    #[test]
    fn test_extract_partition_applied_lsn() {
        let partition_id = PartitionId::from(42); // Example partition ID
        let lsn = Lsn::from(12345); // Example LSN

        let fsm_key = PartitionStateMachineKey {
            partition_id: Some(partition_id.into()),
            state_id: Some(fsm_variable::APPLIED_LSN),
        };

        let mut key_buf = fsm_key.serialize();
        KeyKind::Fsm.serialize(&mut key_buf);
        let key: &[u8] = key_buf.as_ref().into();

        let mut value_buf = BytesMut::new();
        let storage_wrapper: ProtobufStorageWrapper<
            <SequenceNumber as PartitionStoreProtobufValue>::ProtobufType,
        > = ProtobufStorageWrapper(SequenceNumber::from(lsn.as_u64()).into());
        StorageCodec::encode(&storage_wrapper, &mut value_buf)
            .map_err(|e| StorageError::Generic(e.into()))
            .unwrap();
        let value: &[u8] = value_buf.as_ref().into();

        let result = extract_partition_applied_lsn(&key, &value);

        assert_eq!(result, Some((partition_id, lsn)));
    }

    #[test]
    fn test_extract_partition_applied_lsn_invalid_key() {
        let key = b"invalid_key";
        let value = b"some_value";

        let result = extract_partition_applied_lsn(key, value);

        assert!(result.is_none());
    }

    #[test]
    fn test_extract_partition_applied_lsn_incorrect_key() {
        let partition_id = PartitionId::from(42); // Example partition ID
        let lsn = Lsn::from(12345); // Example LSN

        let fsm_key = PartitionStateMachineKey {
            partition_id: Some(partition_id.into()),
            state_id: Some(fsm_variable::INBOX_SEQ_NUMBER),
        };

        let mut key_buf = fsm_key.serialize();
        KeyKind::Fsm.serialize(&mut key_buf);
        let key: &[u8] = key_buf.as_ref().into();

        let mut value_buf = BytesMut::new();
        let storage_wrapper: ProtobufStorageWrapper<
            <SequenceNumber as PartitionStoreProtobufValue>::ProtobufType,
        > = ProtobufStorageWrapper(SequenceNumber::from(lsn.as_u64()).into());
        StorageCodec::encode(&storage_wrapper, &mut value_buf)
            .map_err(|e| StorageError::Generic(e.into()))
            .unwrap();
        let value: &[u8] = value_buf.as_ref().into();

        let result = extract_partition_applied_lsn(key, value);

        assert!(result.is_none());
    }
}
