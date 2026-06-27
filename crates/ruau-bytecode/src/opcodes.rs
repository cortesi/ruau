//! Stable Luau bytecode numeric tags.

use serde::{Deserialize, Serialize};

/// Luau bytecode opcode stored in the low byte of each instruction word.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum Opcode {
    /// No operation.
    Nop = 0,
    /// Debugger break.
    Break = 1,
    /// Load nil.
    LoadNil = 2,
    /// Load boolean.
    LoadB = 3,
    /// Load small number.
    LoadN = 4,
    /// Load constant.
    LoadK = 5,
    /// Move register.
    Move = 6,
    /// Get global.
    GetGlobal = 7,
    /// Set global.
    SetGlobal = 8,
    /// Get upvalue.
    GetUpval = 9,
    /// Set upvalue.
    SetUpval = 10,
    /// Close upvalues.
    CloseUpvals = 11,
    /// Get import.
    GetImport = 12,
    /// Get table by register key.
    GetTable = 13,
    /// Set table by register key.
    SetTable = 14,
    /// Get table by string key.
    GetTableKs = 15,
    /// Set table by string key.
    SetTableKs = 16,
    /// Get table by small integer key.
    GetTableN = 17,
    /// Set table by small integer key.
    SetTableN = 18,
    /// New closure.
    NewClosure = 19,
    /// Method-call preparation.
    NameCall = 20,
    /// Function call.
    Call = 21,
    /// Return from function.
    Return = 22,
    /// Relative jump.
    Jump = 23,
    /// Backedge jump.
    JumpBack = 24,
    /// Conditional truthy jump.
    JumpIf = 25,
    /// Conditional falsey jump.
    JumpIfNot = 26,
    /// Equality jump.
    JumpIfEq = 27,
    /// Less-or-equal jump.
    JumpIfLe = 28,
    /// Less-than jump.
    JumpIfLt = 29,
    /// Inequality jump.
    JumpIfNotEq = 30,
    /// Not less-or-equal jump.
    JumpIfNotLe = 31,
    /// Not less-than jump.
    JumpIfNotLt = 32,
    /// Addition.
    Add = 33,
    /// Subtraction.
    Sub = 34,
    /// Multiplication.
    Mul = 35,
    /// Division.
    Div = 36,
    /// Modulo.
    Mod = 37,
    /// Power.
    Pow = 38,
    /// Addition with constant.
    AddK = 39,
    /// Subtraction with constant.
    SubK = 40,
    /// Multiplication with constant.
    MulK = 41,
    /// Division with constant.
    DivK = 42,
    /// Modulo with constant.
    ModK = 43,
    /// Power with constant.
    PowK = 44,
    /// Logical and.
    And = 45,
    /// Logical or.
    Or = 46,
    /// Logical and with constant.
    AndK = 47,
    /// Logical or with constant.
    OrK = 48,
    /// Concatenation.
    Concat = 49,
    /// Logical not.
    Not = 50,
    /// Numeric negation.
    Minus = 51,
    /// Length.
    Length = 52,
    /// Create table.
    NewTable = 53,
    /// Duplicate table template.
    DupTable = 54,
    /// Set list.
    SetList = 55,
    /// Numeric-for preparation.
    ForNPrep = 56,
    /// Numeric-for loop.
    ForNLoop = 57,
    /// Generic-for loop.
    ForGLoop = 58,
    /// Generic-for preparation for inext.
    ForGPrepInext = 59,
    /// Fastcall with three register arguments.
    FastCall3 = 60,
    /// Generic-for preparation for next.
    ForGPrepNext = 61,
    /// Runtime native call pseudo-instruction.
    NativeCall = 62,
    /// Get varargs.
    GetVarargs = 63,
    /// Duplicate closure.
    DupClosure = 64,
    /// Prepare varargs.
    PrepVarargs = 65,
    /// Load extended constant.
    LoadKx = 66,
    /// Long relative jump.
    JumpX = 67,
    /// Fastcall.
    FastCall = 68,
    /// Coverage counter.
    Coverage = 69,
    /// Upvalue capture.
    Capture = 70,
    /// Constant-minus-register subtraction.
    SubRk = 71,
    /// Constant-divided-by-register division.
    DivRk = 72,
    /// Fastcall with one register argument.
    FastCall1 = 73,
    /// Fastcall with two register arguments.
    FastCall2 = 74,
    /// Fastcall with one register and one constant argument.
    FastCall2K = 75,
    /// Generic-for preparation.
    ForGPrep = 76,
    /// Nil equality jump.
    JumpXEqKNil = 77,
    /// Boolean equality jump.
    JumpXEqKB = 78,
    /// Number equality jump.
    JumpXEqKN = 79,
    /// String equality jump.
    JumpXEqKS = 80,
    /// Floor division.
    IDiv = 81,
    /// Floor division with constant.
    IDivK = 82,
    /// Userdata field get.
    GetUdataKs = 83,
    /// Userdata field set.
    SetUdataKs = 84,
    /// Userdata method-call preparation.
    NameCallUdata = 85,
    /// Register class member.
    NewClassMember = 86,
    /// Call with feedback slot.
    CallFb = 87,
    /// Compare closure proto.
    CmpProto = 88,
}

impl Opcode {
    /// Sentinel one past the largest valid opcode.
    pub const COUNT: u8 = 89;

    /// Converts the serialized opcode byte into a typed opcode.
    #[must_use]
    pub const fn from_byte(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Nop,
            1 => Self::Break,
            2 => Self::LoadNil,
            3 => Self::LoadB,
            4 => Self::LoadN,
            5 => Self::LoadK,
            6 => Self::Move,
            7 => Self::GetGlobal,
            8 => Self::SetGlobal,
            9 => Self::GetUpval,
            10 => Self::SetUpval,
            11 => Self::CloseUpvals,
            12 => Self::GetImport,
            13 => Self::GetTable,
            14 => Self::SetTable,
            15 => Self::GetTableKs,
            16 => Self::SetTableKs,
            17 => Self::GetTableN,
            18 => Self::SetTableN,
            19 => Self::NewClosure,
            20 => Self::NameCall,
            21 => Self::Call,
            22 => Self::Return,
            23 => Self::Jump,
            24 => Self::JumpBack,
            25 => Self::JumpIf,
            26 => Self::JumpIfNot,
            27 => Self::JumpIfEq,
            28 => Self::JumpIfLe,
            29 => Self::JumpIfLt,
            30 => Self::JumpIfNotEq,
            31 => Self::JumpIfNotLe,
            32 => Self::JumpIfNotLt,
            33 => Self::Add,
            34 => Self::Sub,
            35 => Self::Mul,
            36 => Self::Div,
            37 => Self::Mod,
            38 => Self::Pow,
            39 => Self::AddK,
            40 => Self::SubK,
            41 => Self::MulK,
            42 => Self::DivK,
            43 => Self::ModK,
            44 => Self::PowK,
            45 => Self::And,
            46 => Self::Or,
            47 => Self::AndK,
            48 => Self::OrK,
            49 => Self::Concat,
            50 => Self::Not,
            51 => Self::Minus,
            52 => Self::Length,
            53 => Self::NewTable,
            54 => Self::DupTable,
            55 => Self::SetList,
            56 => Self::ForNPrep,
            57 => Self::ForNLoop,
            58 => Self::ForGLoop,
            59 => Self::ForGPrepInext,
            60 => Self::FastCall3,
            61 => Self::ForGPrepNext,
            62 => Self::NativeCall,
            63 => Self::GetVarargs,
            64 => Self::DupClosure,
            65 => Self::PrepVarargs,
            66 => Self::LoadKx,
            67 => Self::JumpX,
            68 => Self::FastCall,
            69 => Self::Coverage,
            70 => Self::Capture,
            71 => Self::SubRk,
            72 => Self::DivRk,
            73 => Self::FastCall1,
            74 => Self::FastCall2,
            75 => Self::FastCall2K,
            76 => Self::ForGPrep,
            77 => Self::JumpXEqKNil,
            78 => Self::JumpXEqKB,
            79 => Self::JumpXEqKN,
            80 => Self::JumpXEqKS,
            81 => Self::IDiv,
            82 => Self::IDivK,
            83 => Self::GetUdataKs,
            84 => Self::SetUdataKs,
            85 => Self::NameCallUdata,
            86 => Self::NewClassMember,
            87 => Self::CallFb,
            88 => Self::CmpProto,
            _ => return None,
        })
    }

    /// Serialized numeric value.
    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Number of 32-bit words occupied by the instruction.
    #[must_use]
    pub const fn instruction_len(self) -> usize {
        match self {
            Self::GetGlobal
            | Self::SetGlobal
            | Self::GetImport
            | Self::GetTableKs
            | Self::SetTableKs
            | Self::NameCall
            | Self::JumpIfEq
            | Self::JumpIfLe
            | Self::JumpIfLt
            | Self::JumpIfNotEq
            | Self::JumpIfNotLe
            | Self::JumpIfNotLt
            | Self::NewTable
            | Self::SetList
            | Self::ForGLoop
            | Self::LoadKx
            | Self::FastCall2
            | Self::FastCall2K
            | Self::FastCall3
            | Self::JumpXEqKNil
            | Self::JumpXEqKB
            | Self::JumpXEqKN
            | Self::JumpXEqKS
            | Self::GetUdataKs
            | Self::SetUdataKs
            | Self::NameCallUdata
            | Self::NewClassMember
            | Self::CallFb
            | Self::CmpProto => 2,
            _ => 1,
        }
    }
}

/// Constant table tag.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum ConstantTag {
    /// Nil constant.
    Nil = 0,
    /// Boolean constant.
    Boolean = 1,
    /// Number constant.
    Number = 2,
    /// String constant.
    String = 3,
    /// Import constant.
    Import = 4,
    /// Table-shape constant.
    Table = 5,
    /// Closure constant.
    Closure = 6,
    /// Vector constant.
    Vector = 7,
    /// Table-shape constant with pre-filled constants.
    TableWithConstants = 8,
    /// 64-bit integer constant.
    Integer = 9,
    /// Class-shape constant.
    ClassShape = 10,
}

impl ConstantTag {
    /// Converts a serialized constant tag.
    #[must_use]
    pub const fn from_byte(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Nil,
            1 => Self::Boolean,
            2 => Self::Number,
            3 => Self::String,
            4 => Self::Import,
            5 => Self::Table,
            6 => Self::Closure,
            7 => Self::Vector,
            8 => Self::TableWithConstants,
            9 => Self::Integer,
            10 => Self::ClassShape,
            _ => return None,
        })
    }
}

/// Bytecode type-info tag.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u16)]
pub enum TypeTag {
    /// Nil type.
    Nil = 0,
    /// Boolean type.
    Boolean = 1,
    /// Number type.
    Number = 2,
    /// String type.
    String = 3,
    /// Table type.
    Table = 4,
    /// Function type.
    Function = 5,
    /// Thread type.
    Thread = 6,
    /// Userdata type.
    Userdata = 7,
    /// Vector type.
    Vector = 8,
    /// Buffer type.
    Buffer = 9,
    /// Integer type.
    Integer = 10,
    /// Any type.
    Any = 15,
    /// First tagged userdata type.
    TaggedUserdataBase = 64,
    /// End of tagged userdata range.
    TaggedUserdataEnd = 96,
    /// Optional-bit mask.
    OptionalBit = 128,
    /// Invalid sentinel.
    Invalid = 256,
}

/// Builtin function ids used by fastcall instructions.
pub struct BuiltinFunction;

#[allow(missing_docs)]
impl BuiltinFunction {
    /// No builtin.
    pub const NONE: u8 = 0;
    /// `assert`.
    pub const ASSERT: u8 = 1;
    pub const MATH_ABS: u8 = 2;
    pub const MATH_ACOS: u8 = 3;
    pub const MATH_ASIN: u8 = 4;
    pub const MATH_ATAN2: u8 = 5;
    pub const MATH_ATAN: u8 = 6;
    pub const MATH_CEIL: u8 = 7;
    pub const MATH_COSH: u8 = 8;
    pub const MATH_COS: u8 = 9;
    pub const MATH_DEG: u8 = 10;
    pub const MATH_EXP: u8 = 11;
    pub const MATH_FLOOR: u8 = 12;
    pub const MATH_FMOD: u8 = 13;
    pub const MATH_FREXP: u8 = 14;
    pub const MATH_LDEXP: u8 = 15;
    pub const MATH_LOG10: u8 = 16;
    pub const MATH_LOG: u8 = 17;
    pub const MATH_MAX: u8 = 18;
    pub const MATH_MIN: u8 = 19;
    pub const MATH_MODF: u8 = 20;
    pub const MATH_POW: u8 = 21;
    pub const MATH_RAD: u8 = 22;
    pub const MATH_SINH: u8 = 23;
    pub const MATH_SIN: u8 = 24;
    pub const MATH_SQRT: u8 = 25;
    pub const MATH_TANH: u8 = 26;
    pub const MATH_TAN: u8 = 27;
    pub const BIT32_ARSHIFT: u8 = 28;
    pub const BIT32_BAND: u8 = 29;
    pub const BIT32_BNOT: u8 = 30;
    pub const BIT32_BOR: u8 = 31;
    pub const BIT32_BXOR: u8 = 32;
    pub const BIT32_BTEST: u8 = 33;
    pub const BIT32_EXTRACT: u8 = 34;
    pub const BIT32_LROTATE: u8 = 35;
    pub const BIT32_LSHIFT: u8 = 36;
    pub const BIT32_REPLACE: u8 = 37;
    pub const BIT32_RROTATE: u8 = 38;
    pub const BIT32_RSHIFT: u8 = 39;
    pub const TYPE: u8 = 40;
    pub const STRING_BYTE: u8 = 41;
    pub const STRING_CHAR: u8 = 42;
    pub const STRING_LEN: u8 = 43;
    pub const TYPEOF: u8 = 44;
    pub const STRING_SUB: u8 = 45;
    pub const MATH_CLAMP: u8 = 46;
    pub const MATH_SIGN: u8 = 47;
    pub const MATH_ROUND: u8 = 48;
    pub const RAWSET: u8 = 49;
    pub const RAWGET: u8 = 50;
    pub const RAWEQUAL: u8 = 51;
    pub const TABLE_INSERT: u8 = 52;
    pub const TABLE_UNPACK: u8 = 53;
    pub const VECTOR: u8 = 54;
    pub const BIT32_COUNTLZ: u8 = 55;
    pub const BIT32_COUNTRZ: u8 = 56;
    pub const SELECT_VARARG: u8 = 57;
    pub const RAWLEN: u8 = 58;
    pub const BIT32_EXTRACTK: u8 = 59;
    pub const GETMETATABLE: u8 = 60;
    pub const SETMETATABLE: u8 = 61;
    pub const TONUMBER: u8 = 62;
    pub const TOSTRING: u8 = 63;
    pub const BIT32_BYTESWAP: u8 = 64;
    pub const BUFFER_READI8: u8 = 65;
    pub const BUFFER_READU8: u8 = 66;
    pub const BUFFER_WRITEU8: u8 = 67;
    pub const BUFFER_READI16: u8 = 68;
    pub const BUFFER_READU16: u8 = 69;
    pub const BUFFER_WRITEU16: u8 = 70;
    pub const BUFFER_READI32: u8 = 71;
    pub const BUFFER_READU32: u8 = 72;
    pub const BUFFER_WRITEU32: u8 = 73;
    pub const BUFFER_READF32: u8 = 74;
    pub const BUFFER_WRITEF32: u8 = 75;
    pub const BUFFER_READF64: u8 = 76;
    pub const BUFFER_WRITEF64: u8 = 77;
    pub const VECTOR_MAGNITUDE: u8 = 78;
    pub const VECTOR_NORMALIZE: u8 = 79;
    pub const VECTOR_CROSS: u8 = 80;
    pub const VECTOR_DOT: u8 = 81;
    pub const VECTOR_FLOOR: u8 = 82;
    pub const VECTOR_CEIL: u8 = 83;
    pub const VECTOR_ABS: u8 = 84;
    pub const VECTOR_SIGN: u8 = 85;
    pub const VECTOR_CLAMP: u8 = 86;
    pub const VECTOR_MIN: u8 = 87;
    pub const VECTOR_MAX: u8 = 88;
    pub const MATH_LERP: u8 = 89;
    pub const VECTOR_LERP: u8 = 90;
    pub const MATH_ISNAN: u8 = 91;
    pub const MATH_ISINF: u8 = 92;
    pub const MATH_ISFINITE: u8 = 93;
    pub const INTEGER_CREATE: u8 = 94;
    pub const INTEGER_TONUMBER: u8 = 95;
    pub const INTEGER_NEG: u8 = 96;
    pub const INTEGER_ADD: u8 = 97;
    pub const INTEGER_SUB: u8 = 98;
    pub const INTEGER_MUL: u8 = 99;
    pub const INTEGER_DIV: u8 = 100;
    pub const INTEGER_MIN: u8 = 101;
    pub const INTEGER_MAX: u8 = 102;
    pub const INTEGER_REM: u8 = 103;
    pub const INTEGER_IDIV: u8 = 104;
    pub const INTEGER_UDIV: u8 = 105;
    pub const INTEGER_UREM: u8 = 106;
    pub const INTEGER_MOD: u8 = 107;
    pub const INTEGER_CLAMP: u8 = 108;
    pub const INTEGER_BAND: u8 = 109;
    pub const INTEGER_BOR: u8 = 110;
    pub const INTEGER_BNOT: u8 = 111;
    pub const INTEGER_BXOR: u8 = 112;
    pub const INTEGER_LT: u8 = 113;
    pub const INTEGER_LE: u8 = 114;
    pub const INTEGER_ULT: u8 = 115;
    pub const INTEGER_ULE: u8 = 116;
    pub const INTEGER_GT: u8 = 117;
    pub const INTEGER_GE: u8 = 118;
    pub const INTEGER_UGT: u8 = 119;
    pub const INTEGER_UGE: u8 = 120;
    pub const INTEGER_LSHIFT: u8 = 121;
    pub const INTEGER_RSHIFT: u8 = 122;
    pub const INTEGER_ARSHIFT: u8 = 123;
    pub const INTEGER_LROTATE: u8 = 124;
    pub const INTEGER_RROTATE: u8 = 125;
    pub const INTEGER_EXTRACT: u8 = 126;
    pub const INTEGER_BTEST: u8 = 127;
    pub const INTEGER_COUNTRZ: u8 = 128;
    pub const INTEGER_COUNTLZ: u8 = 129;
    pub const INTEGER_BSWAP: u8 = 130;
    pub const BUFFER_READINTEGER: u8 = 131;
    pub const BUFFER_WRITEINTEGER: u8 = 132;

    /// Sentinel one past the largest builtin function id — the size of the
    /// fastcall id space, mirroring [`Opcode::COUNT`].
    pub const COUNT: u8 = 133;
}

/// Capture type used by `CAPTURE`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum CaptureType {
    /// Capture value.
    Val = 0,
    /// Capture by reference.
    Ref = 1,
    /// Capture upvalue.
    Upval = 2,
}

impl CaptureType {
    /// Converts a serialized capture tag.
    #[must_use]
    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Val),
            1 => Some(Self::Ref),
            2 => Some(Self::Upval),
            _ => None,
        }
    }
}

/// `JumpXEqK*` AUX word: bit 31 negates the comparison.
pub const JUMPX_K_NOT_BIT: u32 = 1 << 31;
/// `JumpXEqK*` AUX word: the low 24 bits carry the constant index.
pub const JUMPX_K_INDEX_MASK: u32 = (1 << 24) - 1;

/// `FORGLOOP` AUX word: bit 31 marks an `ipairs`-style inext fast path
/// (set when the loop was prepared by `ForGPrepInext`).
pub const FORGLOOP_INEXT_BIT: u32 = 1 << 31;
/// `FORGLOOP` AUX word: the low byte carries the iteration variable count.
pub const FORGLOOP_VARS_MASK: u32 = 0xff;

/// Import id: bits 30-31 carry the path-component count (1-3).
pub const IMPORT_PATH_COUNT_SHIFT: u32 = 30;
/// Import id: each path component is a 10-bit string-constant index,
/// packed high-to-low starting at bit 20.
pub const IMPORT_PATH_COMPONENT_BITS: u32 = 10;
/// Mask for one packed import path component.
pub const IMPORT_PATH_COMPONENT_MASK: u32 = (1 << IMPORT_PATH_COMPONENT_BITS) - 1;

/// Bit shift of the `index`-th (0-based) packed import path component.
#[must_use]
pub const fn import_component_shift(index: u32) -> u32 {
    IMPORT_PATH_COUNT_SHIFT - IMPORT_PATH_COMPONENT_BITS * (index + 1)
}

/// Proto flag bitmask values.
pub struct ProtoFlag;

impl ProtoFlag {
    /// Function can be inlined. (Bits 0-2 are upstream's native-codegen
    /// flags — `NATIVE_MODULE`/`NATIVE_COLD`/`NATIVE_FUNCTION` — which this
    /// compiler never sets; the encoded flag byte carries them verbatim.)
    pub const INLINABLE: u8 = 1 << 3;
}

/// Feedback-slot type tags.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum FeedbackType {
    /// Call-target feedback.
    CallTarget = 0,
}

impl FeedbackType {
    /// Converts a serialized feedback tag.
    #[must_use]
    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::CallTarget),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BuiltinFunction, CaptureType, ConstantTag, FeedbackType, Opcode, ProtoFlag, TypeTag,
    };

    macro_rules! upstream_case {
        ($case:literal) => {};
    }

    #[test]
    fn opcode_numbers_match_upstream() {
        upstream_case!("Compiler.test.cpp::Compiler::BytecodeIsStable");
        assert_eq!(Opcode::Nop.byte(), 0);
        assert_eq!(Opcode::Return.byte(), 22);
        assert_eq!(Opcode::FastCall3.byte(), 60);
        assert_eq!(Opcode::CmpProto.byte(), 88);
        assert_eq!(Opcode::COUNT, 89);
    }

    #[test]
    fn other_bytecode_tags_match_upstream() {
        assert_eq!(ConstantTag::Nil as u8, 0);
        assert_eq!(ConstantTag::Integer as u8, 9);
        assert_eq!(ConstantTag::ClassShape as u8, 10);
        assert_eq!(TypeTag::Any as u16, 15);
        assert_eq!(TypeTag::OptionalBit as u16, 128);
        assert_eq!(TypeTag::Invalid as u16, 256);
        assert_eq!(CaptureType::Upval as u8, 2);
        assert_eq!(ProtoFlag::INLINABLE, 8);
        assert_eq!(FeedbackType::CallTarget as u8, 0);
        assert_eq!(BuiltinFunction::BUFFER_WRITEINTEGER, 132);
        assert_eq!(BuiltinFunction::COUNT, 133);
    }
}
