use std::{fmt, result::Result as StdResult};

use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
    ser::{Serialize, SerializeTupleStruct, Serializer},
};

/// A Luau vector type.
#[derive(Debug, Default, Clone, Copy, PartialEq, PartialOrd)]
pub struct Vector(pub(crate) [f32; Self::SIZE]);

impl fmt::Display for Vector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vector({}, {}, {})", self.x(), self.y(), self.z())
    }
}

impl Vector {
    /// Number of vector components supported by Luau.
    pub(crate) const SIZE: usize = 3;

    /// Creates a new vector.
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self([x, y, z])
    }

    /// Creates a new vector with all components set to `0.0`.
    pub const fn zero() -> Self {
        Self([0.0; Self::SIZE])
    }

    /// Returns 1st component of the vector.
    pub const fn x(&self) -> f32 {
        self.0[0]
    }

    /// Returns 2nd component of the vector.
    pub const fn y(&self) -> f32 {
        self.0[1]
    }

    /// Returns 3rd component of the vector.
    pub const fn z(&self) -> f32 {
        self.0[2]
    }
}

impl From<[f32; 3]> for Vector {
    fn from(value: [f32; 3]) -> Self {
        Self(value)
    }
}

impl From<Vector> for [f32; 3] {
    fn from(value: Vector) -> Self {
        value.0
    }
}

impl Serialize for Vector {
    fn serialize<S: Serializer>(&self, serializer: S) -> StdResult<S::Ok, S::Error> {
        let mut ts = serializer.serialize_tuple_struct("Vector", Self::SIZE)?;
        ts.serialize_field(&self.x())?;
        ts.serialize_field(&self.y())?;
        ts.serialize_field(&self.z())?;
        ts.end()
    }
}

impl<'de> Deserialize<'de> for Vector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> StdResult<Self, D::Error> {
        struct VectorVisitor;

        impl<'de> Visitor<'de> for VectorVisitor {
            type Value = Vector;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Luau vector represented as three f32 components")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> StdResult<Self::Value, A::Error> {
                let x = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let y = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let z = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                Ok(Vector::new(x, y, z))
            }
        }

        deserializer.deserialize_tuple_struct("Vector", Self::SIZE, VectorVisitor)
    }
}

impl PartialEq<[f32; Self::SIZE]> for Vector {
    #[inline]
    fn eq(&self, other: &[f32; Self::SIZE]) -> bool {
        self.0 == *other
    }
}
