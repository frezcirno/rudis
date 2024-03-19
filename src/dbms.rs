use crate::aof::{AofState, AofWriter};
use crate::config::ConfigRef;
use crate::object::RudisObject;
use crate::rdb::Rdb;
use crate::shared;
use bytes::{Bytes, BytesMut};
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::{Ref, RefMut};
use dashmap::DashMap;
use std::io::ErrorKind;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::fs::File;
use tokio::task::JoinHandle;

static mut ID: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
pub struct DatabaseManager {
    pub config: ConfigRef,
    pub dbs: DatabaseRef, // only one database for now

    pub clock_ms: u64,

    pub last_save_time: u64,
    pub rdb_save_task: Option<JoinHandle<()>>,

    pub dirty: u64,
    pub dirty_before_bgsave: u64,

    pub aof_buf: AofWriter,
    pub aof_file: Option<File>,
    pub aof_current_size: u64,       // current size of the aof file
    pub aof_last_write_status: bool, // true if last write was ok

    pub aof_selected_db: Option<u32>,

    pub aof_last_fsync: u64, // unit: ms

    pub aof_rewrite_task: Option<JoinHandle<()>>,
    pub aof_rewrite_buf_blocks: BytesMut,
    pub aof_rewrite_scheduled: bool,
}

impl DatabaseManager {
    pub async fn new(config: ConfigRef) -> DatabaseManager {
        // only one database for now
        // let db_num = config.read().await.db_num;
        // let mut v = Vec::with_capacity(db_num);
        // for _ in 0..db_num {
        //     v.push(Database::new());
        // }
        DatabaseManager {
            config,
            dbs: DatabaseRef::new(),
            ..Default::default()
        }
    }

    // pub fn len(&self) -> usize {
    //     self.inner.len()
    // }

    pub fn get(&self, _index: usize) -> DatabaseRef {
        // self.inner[index].clone()
        self.dbs.clone()
    }

    pub fn clone(&self) -> DatabaseManager {
        DatabaseManager {
            dbs: self.dbs.clone(),
            ..Default::default()
        }
    }

    pub async fn load_data_from_disk(&mut self) {
        if self.config.read().await.aof_state == AofState::On {
            // TODO
        } else {
            match File::open(&self.config.clone().read().await.rdb_filename).await {
                Ok(file) => match self.load(&mut Rdb::from_file(file)).await {
                    Ok(()) => log::info!("DB loaded from disk"),
                    Err(e) => log::error!("Error loading DB from disk: {:?}", e),
                },
                Err(err) => {
                    if err.kind() != ErrorKind::NotFound {
                        log::error!("Error loading DB from disk: {:?}", err);
                    }
                }
            }
        }
    }

    pub async fn should_save(&self) -> bool {
        let time_to_last_save = self.clock_ms - self.last_save_time;
        for saveparam in &self.config.read().await.save_params {
            if self.dirty >= saveparam.changes && time_to_last_save >= saveparam.seconds {
                return true;
            }
        }
        false
    }
}

impl Deref for DatabaseManager {
    type Target = DatabaseRef;

    fn deref(&self) -> &Self::Target {
        &self.dbs
    }
}

#[derive(Default, Clone)]
pub struct DatabaseRef {
    pub index: u32,
    inner: Arc<Dict>,
}

impl DatabaseRef {
    pub fn new() -> DatabaseRef {
        DatabaseRef {
            index: unsafe { ID.fetch_add(1, Ordering::Relaxed) },
            inner: Arc::new(Dict::new()),
        }
    }
}

impl Deref for DatabaseRef {
    type Target = Arc<Dict>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for DatabaseRef {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Debug, Clone)]
pub struct DictValue {
    pub value: RudisObject,
    pub expire_at: Option<u64>,
}

impl DictValue {
    pub fn new(value: RudisObject, expire_at: Option<u64>) -> DictValue {
        DictValue { value, expire_at }
    }

    pub fn is_volatile(&self) -> bool {
        self.expire_at.is_some()
    }

    pub fn is_expired(&self) -> bool {
        if let Some(expire) = self.expire_at {
            let now = shared::now_ms();
            if now > expire {
                return true;
            }
        }
        false
    }
}

#[derive(Default)]
pub struct Dict {
    pub dict: DashMap<Bytes, DictValue>, // millisecond timestamp
}

impl Dict {
    pub fn new() -> Dict {
        Dict {
            dict: DashMap::new(),
        }
    }

    pub fn check_expired(&self, key: &Bytes) {
        if let Some(entry) = self.dict.get(key) {
            if entry.is_expired() {
                self.dict.remove(key);
            }
        }
    }

    pub fn get(&self, key: &Bytes) -> Option<Ref<'_, Bytes, DictValue>> {
        self.check_expired(key);
        self.dict.get(key)
    }

    pub fn get_mut(&self, key: &Bytes) -> Option<RefMut<'_, Bytes, DictValue>> {
        self.check_expired(key);
        self.dict.get_mut(key)
    }

    pub fn remove(&self, key: &Bytes) -> Option<(Bytes, DictValue)> {
        self.check_expired(key);
        self.dict.remove(key)
    }

    pub fn entry(&self, key: Bytes) -> Entry<'_, Bytes, DictValue> {
        self.check_expired(&key);
        self.dict.entry(key)
    }

    pub fn contains_key(&self, key: &Bytes) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(
        &self,
        key: Bytes,
        value: RudisObject,
        expire_at: Option<u64>,
    ) -> Option<DictValue> {
        self.dict.insert(key, DictValue::new(value, expire_at))
    }

    pub fn rename(&self, key: &Bytes, new_key: Bytes) -> bool {
        if let Some(v) = self.dict.remove(key) {
            self.dict.insert(new_key.clone(), v.1);
            true
        } else {
            false
        }
    }

    pub fn expire_at(&self, key: &Bytes, expire_at_ms: u64) -> bool {
        if let Some(mut v) = self.dict.get_mut(key) {
            v.expire_at = Some(expire_at_ms);
            true
        } else {
            false
        }
    }
}

impl Deref for Dict {
    type Target = DashMap<Bytes, DictValue>;

    fn deref(&self) -> &Self::Target {
        &self.dict
    }
}

impl DerefMut for Dict {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dict
    }
}
