use super::CommandParser;
use crate::client::Client;
use crate::dbms::DictValue;
use crate::frame::Frame;
use crate::object::RudisObject;
use crate::shared;
use bytes::{Bytes, BytesMut};
use dashmap::mapref::entry::Entry;
use std::io::{Error, ErrorKind, Result};

#[derive(Debug, Clone)]
pub struct Get {
    pub key: Bytes,
}

impl Get {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "GET requires a key"))?;
        Ok(Self { key })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        // Get the value from the shared database state
        let response = {
            if let Some(entry) = client.db.get(&self.key) {
                // If a value is present, it is written to the client in "bulk"
                // format.
                entry.value.serialize()
            } else {
                // If there is no value, `Null` is written.
                Frame::Null
            }
        };

        // Write the response back to the client
        client.write_frame(&response).await?;

        Ok(())
    }
}

const REDIS_SET_NO_FLAGS: u32 = 0;
const REDIS_SET_NX: u32 = 1 << 0; /* Set if key not exists. */
const REDIS_SET_XX: u32 = 1 << 1; /* Set if key exists. */

#[derive(Debug, Clone)]
pub struct Set {
    pub key: Bytes,
    pub val: BytesMut,
    pub flags: u32,
    pub expire: Option<u64>, // milliseconds
}

async fn generic_set(
    key: Bytes,
    val: BytesMut,
    flags: u32,
    expire: Option<u64>,
    client: &mut Client,
) -> Result<()> {
    match client.db.clone().entry(key) {
        Entry::Occupied(mut oe) => {
            if flags & REDIS_SET_NX != 0 {
                client.write_frame(&shared::null_bulk).await.unwrap();
                return Ok(());
            }

            {
                let entry = oe.get_mut();
                entry.value = RudisObject::new_string_from(val);
                entry.expire_at = expire.map(|ms| shared::now_ms() + ms);
            }

            drop(oe);

            client.write_frame(&shared::ok).await.unwrap();

            Ok(())
        }
        Entry::Vacant(ve) => {
            if flags & REDIS_SET_XX != 0 {
                client.write_frame(&shared::null_bulk).await.unwrap();
                return Ok(());
            }

            ve.insert(DictValue::new(
                RudisObject::new_string_from(val),
                expire.map(|ms| shared::now_ms() + ms),
            ));

            client.write_frame(&shared::ok).await.unwrap();

            Ok(())
        }
    }
}

impl Set {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        if frame.remaining() < 2 {
            return Err(Error::new(ErrorKind::Other, shared::syntax_err.to_string()));
        }
        // The first two elements of the array are the key and value
        let key = frame.next_string()?.unwrap();
        let val = frame.next_string()?.unwrap();

        let mut flags = REDIS_SET_NO_FLAGS;
        let mut expire = None;

        while frame.has_next() {
            let val = frame.next_string()?.unwrap().to_ascii_lowercase();
            if &val == b"nx" {
                flags |= REDIS_SET_NX;
            } else if &val == b"xx" {
                flags |= REDIS_SET_XX;
            } else if &val == b"ex" {
                // expire time in seconds
                let time = {
                    if let Some(maybe_time) = frame.next_integer()? {
                        maybe_time as u64
                    } else {
                        return Err(Error::new(ErrorKind::Other, shared::syntax_err.to_string()));
                    }
                };
                expire = Some(time * 1000);
            } else if &val == b"px" {
                // expire time in milliseconds
                let time = {
                    if let Some(maybe_time) = frame.next_integer()? {
                        maybe_time as u64
                    } else {
                        return Err(Error::new(ErrorKind::Other, shared::syntax_err.to_string()));
                    }
                };
                expire = Some(time);
            } else {
                // error
                return Err(Error::new(ErrorKind::Other, shared::syntax_err.to_string()));
            }
        }

        Ok(Self {
            key,
            val: BytesMut::from(&val[..]),
            flags,
            expire,
        })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_set(self.key, self.val, self.flags, self.expire, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 3 + if self.expire.is_some() { 2 } else { 0 });
        shared::extend_bulk_string(&mut out, b"SET" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, &self.val[..]);
        if let Some(expire) = self.expire {
            shared::extend_bulk_string(&mut out, b"PX" as &[u8]);
            shared::extend_bulk_string(&mut out, expire.to_string().as_bytes());
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct SetNx {
    pub key: Bytes,
    pub val: BytesMut,
}

impl SetNx {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        if frame.remaining() < 2 {
            return Err(Error::new(ErrorKind::Other, shared::syntax_err.to_string()));
        }
        // The first two elements of the array are the key and value
        let key = frame.next_string()?.unwrap();
        let val = frame.next_string()?.unwrap();

        Ok(Self {
            key,
            val: BytesMut::from(&val[..]),
        })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_set(self.key, self.val, REDIS_SET_NX, None, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 3);
        shared::extend_bulk_string(&mut out, b"SETNX" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, &self.val[..]);
        out
    }
}

async fn generic_incdec(key: Bytes, incr: i64, client: &mut Client) -> Result<()> {
    // Increment the value in the shared database state
    let response = {
        let mut value = client.db.entry(key).or_insert_with(|| {
            DictValue::new(RudisObject::new_string_from(BytesMut::from("0")), None)
        });
        if let RudisObject::String(s) = &mut value.value {
            if let Some(n) = s.parse_int() {
                s.value = BytesMut::from((n + incr).to_string().as_bytes());
                Frame::Integer(n + incr)
            } else {
                Frame::Error(Bytes::from_static(
                    b"ERR value is not an integer or out of range",
                ))
            }
        } else {
            Frame::Error(Bytes::from_static(
                b"Operation against a key holding the wrong kind of value",
            ))
        }
    };

    // Write the response back to the client
    client.write_frame(&response).await?;

    Ok(())
}

#[derive(Debug, Clone)]
pub struct Incr {
    pub key: Bytes,
}

impl Incr {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "INCR requires a key"))?;
        Ok(Self { key })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_incdec(self.key, 1, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 2);
        shared::extend_bulk_string(&mut out, b"INCR" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        out
    }
}

#[derive(Debug, Clone)]
pub struct IncrBy {
    pub key: Bytes,
    pub increment: i64,
}

impl IncrBy {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "INCRBY requires a key"))?;
        let increment = frame
            .next_integer()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "INCRBY requires an increment"))?;
        Ok(Self { key, increment })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_incdec(self.key, self.increment, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 3);
        shared::extend_bulk_string(&mut out, b"INCRBY" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, self.increment.to_string().as_bytes());
        out
    }
}

#[derive(Debug, Clone)]
pub struct Decr {
    pub key: Bytes,
}

impl Decr {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "DECR requires a key"))?;
        Ok(Self { key })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_incdec(self.key, -1, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 2);
        shared::extend_bulk_string(&mut out, b"DECR" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        out
    }
}

#[derive(Debug, Clone)]
pub struct DecrBy {
    pub key: Bytes,
    pub decrement: i64,
}

impl DecrBy {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "DECRBY requires a key"))?;
        let decrement = frame
            .next_integer()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "DECRBY requires a decrement"))?;
        Ok(Self { key, decrement })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        generic_incdec(self.key, -self.decrement, client).await
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 3);
        shared::extend_bulk_string(&mut out, b"DECRBY" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, self.decrement.to_string().as_bytes());
        out
    }
}

#[derive(Debug, Clone)]
pub struct Append {
    pub key: Bytes,
    pub value: Bytes,
}

impl Append {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "APPEND requires a key"))?;
        let value = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "APPEND requires a value"))?;
        if value.len() > 512 * 1024 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "value is too large (maximum 512MB)",
            ));
        }
        Ok(Self { key, value })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        // Append the value to the shared database state
        let response = {
            // locked write
            match client.db.entry(self.key) {
                Entry::Occupied(mut oe) => {
                    if let RudisObject::String(s) = &mut oe.get_mut().value {
                        s.extend_from_slice(&self.value);
                        Frame::Integer(s.len() as i64)
                    } else {
                        Frame::Error(Bytes::from_static(
                            b"Operation against a key holding the wrong kind of value",
                        ))
                    }
                }
                Entry::Vacant(ve) => {
                    ve.insert(DictValue::new(
                        RudisObject::new_string_from(BytesMut::from(&self.value[..])),
                        None,
                    ));
                    Frame::Integer(self.value.len() as i64)
                }
            }
        };

        // Write the response back to the client
        client.write_frame(&response).await?;

        Ok(())
    }

    pub fn rewrite(&self) -> BytesMut {
        let mut out = BytesMut::new();
        shared::extend_array(&mut out, 3);
        shared::extend_bulk_string(&mut out, b"APPEND" as &[u8]);
        shared::extend_bulk_string(&mut out, &self.key[..]);
        shared::extend_bulk_string(&mut out, &self.value[..]);
        out
    }
}

#[derive(Debug, Clone)]
pub struct Strlen {
    pub key: Bytes,
}

impl Strlen {
    pub fn from(frame: &mut CommandParser) -> Result<Self> {
        let key = frame
            .next_string()?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "STRLEN requires a key"))?;
        Ok(Self { key })
    }

    pub async fn apply(self, client: &mut Client) -> Result<()> {
        // Get the value from the shared database state
        let response = {
            if let Some(entry) = client.db.get(&self.key) {
                if let RudisObject::String(s) = &entry.value {
                    Frame::Integer(s.len() as i64)
                } else {
                    Frame::Error(Bytes::from_static(
                        b"Operation against a key holding the wrong kind of value",
                    ))
                }
            } else {
                Frame::Integer(0)
            }
        };

        // Write the response back to the client
        client.write_frame(&response).await?;

        Ok(())
    }
}
