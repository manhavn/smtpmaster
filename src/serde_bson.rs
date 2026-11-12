//! Serde adapters that persist `chrono::DateTime<Utc>` as native BSON dates.
//!
//! `bson` serialises `chrono::DateTime` as an ISO-8601 string by default, which
//! breaks MongoDB date arithmetic and range queries. These adapters convert
//! through [`bson::DateTime`] so the stored type is always BSON `date`.

pub mod datetime {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bson::DateTime::from(*value).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        Ok(bson::DateTime::deserialize(deserializer)?.to_chrono())
    }
}

pub mod datetime_opt {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(bson::DateTime::from).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        Ok(Option::<bson::DateTime>::deserialize(deserializer)?.map(bson::DateTime::to_chrono))
    }
}
