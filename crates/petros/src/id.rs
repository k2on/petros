//! The identity of a log entry.

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql, FromSqlRow};
use diesel::expression::AsExpression;
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::Binary;
use diesel::sqlite::Sqlite;
use serde::{Deserialize, Serialize};

/// A UUID, stored as a sixteen byte blob.
///
/// Diesel maps `uuid::Uuid` for PostgreSQL only, so SQLite needs a newtype that
/// carries the mapping. It is `#[serde(transparent)]`, so it encodes exactly as
/// the bare UUID did and logs written before it existed still decode.
///
/// ```
/// # use petros::Id;
/// let id = Id::from_u128(1);
/// assert_eq!(petros::encode(&id)?, petros::encode(&uuid::Uuid::from_u128(1))?);
/// # Ok::<(), petros::Error>(())
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    AsExpression,
    FromSqlRow,
)]
#[diesel(sql_type = Binary)]
#[serde(transparent)]
pub struct Id(pub uuid::Uuid);

impl Id {
    /// The all-zero id, used as a placeholder before `fill_auto` runs.
    pub const fn nil() -> Self {
        Id(uuid::Uuid::nil())
    }

    pub const fn from_u128(v: u128) -> Self {
        Id(uuid::Uuid::from_u128(v))
    }

    pub fn is_nil(&self) -> bool {
        self.0.is_nil()
    }

    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl From<uuid::Uuid> for Id {
    fn from(u: uuid::Uuid) -> Self {
        Id(u)
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ToSql<Binary, Sqlite> for Id {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        out.set_value(self.0.as_bytes().to_vec());
        Ok(IsNull::No)
    }
}

impl<DB: Backend> FromSql<Binary, DB> for Id
where
    Vec<u8>: FromSql<Binary, DB>,
{
    fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
        let raw = <Vec<u8> as FromSql<Binary, DB>>::from_sql(bytes)?;
        Ok(Id(uuid::Uuid::from_slice(&raw)?))
    }
}
