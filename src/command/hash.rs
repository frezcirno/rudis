use super::CommandParser;
use crate::db::Database;
use crate::object::RudisObject;
use crate::shared;
use crate::{connection::Connection, frame::Frame};
use bytes::{Bytes, BytesMut};
use std::io::{Error, ErrorKind, Result};

#[derive(Debug, Clone)]
pub struct HSet {
    pub key: Bytes,
    pub field: Bytes,
    pub value: Bytes,
}

impl HSet {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HSET requires a key"))?;
        let field = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HSET requires a field"))?;
        let value = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HSET requires a value"))?;
        Ok(Self { key, field, value })
    }

    pub async fn apply(self, db: &Database, dst: &mut Connection) -> Result<()> {
        let mut db = db.lock().await;

        match db.lookup_write(&self.key.clone()) {
            Some(RudisObject::Hash(h)) => {
                h.insert(self.field, BytesMut::from_iter(self.value));
                dst.write_frame(&Frame::Integer(1)).await?;
                Ok(())
            }
            Some(_) => {
                dst.write_frame(&shared::wrong_type_err).await?;
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "WRONGTYPE Operation against a key holding the wrong kind of value",
                ));
            }
            None => {
                let mut h = std::collections::HashMap::new();
                h.insert(self.field, BytesMut::from_iter(self.value));
                db.insert(self.key.clone(), RudisObject::new_hash_from(h), None);
                dst.write_frame(&Frame::Integer(1)).await?;
                Ok(())
            }
        }
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 4);
        shared::extend_bulk_string(&mut out, b"HSET" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, &self.field[..]);
        shared::extend_bulk_string(&mut out, &self.value[..]);
        out
    }
}

#[derive(Debug, Clone)]
pub struct HGet {
    pub key: Bytes,
    pub field: Bytes,
}

impl HGet {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HGET requires a key"))?;
        let field = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HGET requires a field"))?;
        Ok(Self { key, field })
    }

    pub async fn apply(self, db: &Database, dst: &mut Connection) -> Result<()> {
        let mut db = db.lock().await;

        match db.lookup_read(&self.key) {
            Some(RudisObject::Hash(h)) => {
                if let Some(value) = h.get(&self.field) {
                    dst.write_frame(&Frame::Bulk(value.clone().freeze()))
                        .await?;
                } else {
                    dst.write_frame(&Frame::Null).await?;
                }
                Ok(())
            }
            Some(_) => {
                dst.write_frame(&shared::wrong_type_err).await?;
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "WRONGTYPE Operation against a key holding the wrong kind of value",
                ));
            }
            None => {
                dst.write_frame(&Frame::Null).await?;
                Ok(())
            }
        }
    }
}
