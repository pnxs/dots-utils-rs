//! Static inspection of dots-rs ELF binaries.
//!
//! `#[derive(DotsStruct)]` emits per-type registration statics into the
//! `linkme_PUBLISHED_TYPES` / `linkme_SUBSCRIBED_TYPES` ELF sections
//! (see dots-rs-core's `registration` module); on release builds with
//! LTO the linker culls entries for types the binary never publishes or
//! subscribes to. Each section is therefore the binary's own pub/sub
//! type manifest: an array of pointers to `StructDescriptor` statics.
//!
//! This crate reads that manifest from a compiled binary *without
//! executing it*: it applies the dynamic `RELATIVE` relocations a PIE
//! binary needs to make stored pointers meaningful, follows each
//! descriptor pointer, and recovers the full type schema — name, flags,
//! properties, nested struct/enum types — which [`TypeInfo::to_idl`]
//! can render back into `.dots` IDL source.
//!
//! Layout contract: the descriptor types (`StructDescriptor`,
//! `PropertyDescriptor`, `FieldKind`, `EnumDescriptor`, `EnumElement`)
//! are `repr(C)` exactly so this parser can exist, and the offsets and
//! strides used here come from `offset_of!`/`size_of` on the real types
//! — the parser tracks the source of truth automatically. Two de-facto
//! assumptions remain: the (ptr, len) order inside `&str` / `&[T]` fat
//! pointers (never varied in practice; plausibility checks turn a
//! change into a hard error), and `FieldKind`'s discriminant values,
//! which core declares explicitly and append-only.
//!
//! Both the inspecting host and the inspected binary must be 64-bit
//! (any combination of supported architectures and endiannesses works;
//! `usize`/pointer fields are always 8 bytes on both sides).

use core::mem::{offset_of, size_of};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use dots_rs_core::{EnumDescriptor, EnumElement, PropertyDescriptor, StructDescriptor, StructFlags};
use object::{
    Architecture, BinaryFormat, Object, ObjectSection, ObjectSegment, RelocationFlags,
};

/// ELF section holding the published-types slice (linkme's naming for
/// dots-rs-core's `PUBLISHED_TYPES`).
pub const PUBLISHED_SECTION: &str = "linkme_PUBLISHED_TYPES";
/// ELF section holding the subscribed-types slice.
pub const SUBSCRIBED_SECTION: &str = "linkme_SUBSCRIBED_TYPES";
/// ELF section holding the filtered-subscribed-types slice (`View<T>`
/// intent — descriptor announced, no unfiltered group join).
pub const SUBSCRIBED_FILTERED_SECTION: &str = "linkme_SUBSCRIBED_FILTERED_TYPES";

/// One DOTS struct type recovered from the binary.
#[derive(Debug, Clone)]
pub struct TypeInfo {
    /// DOTS type name (the `#[dots(name = "...")]` string).
    pub name: String,
    /// Struct-level flags (`cached`, `persistent`, ...).
    pub flags: StructFlags,
    /// `size_of` of the generated Rust struct in the inspected binary.
    pub size: u64,
    /// `align_of` of the generated Rust struct.
    pub align: u64,
    /// The DOTS properties, in declaration order.
    pub properties: Vec<PropertyInfo>,
    /// Virtual address of the `StructDescriptor` static (useful for
    /// cross-referencing with `objdump`).
    pub descriptor_addr: u64,
}

/// One DOTS property (struct field).
#[derive(Debug, Clone)]
pub struct PropertyInfo {
    pub name: String,
    /// DOTS wire tag (1-based).
    pub tag: u32,
    /// Whether the property participates in the primary key.
    pub is_key: bool,
    pub kind: KindInfo,
}

/// A property's type, mirroring dots-rs-core's `FieldKind` with names
/// resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindInfo {
    /// A built-in type, stored as its `.dots` IDL spelling
    /// (`"uint32"`, `"string"`, ...).
    Scalar(&'static str),
    Vec(Box<KindInfo>),
    /// Nested DOTS struct, by type name.
    Struct(String),
    /// DOTS enum, by type name.
    Enum(String),
}

impl std::fmt::Display for KindInfo {
    /// The `.dots` IDL spelling of the type.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scalar(name) => f.write_str(name),
            Self::Vec(inner) => write!(f, "vector<{inner}>"),
            Self::Struct(name) | Self::Enum(name) => f.write_str(name),
        }
    }
}

/// One DOTS enum type recovered from the binary.
#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub name: String,
    pub elements: Vec<EnumElementInfo>,
    pub descriptor_addr: u64,
}

/// One DOTS enum variant.
#[derive(Debug, Clone)]
pub struct EnumElementInfo {
    pub name: String,
    /// DOTS metadata tag (1-based; the wire encoding).
    pub tag: u32,
    /// Associated `int32` metadata value (defaults to `tag - 1`).
    pub value: i32,
}

/// Everything recovered from one binary.
#[derive(Debug, Default)]
pub struct Report {
    /// Types the binary publishes, sorted by name.
    pub published: Vec<TypeInfo>,
    /// Types the binary subscribes to, sorted by name.
    pub subscribed: Vec<TypeInfo>,
    /// Types the binary opens filtered subscriptions (`View<T>`) on,
    /// sorted by name.
    pub subscribed_filtered: Vec<TypeInfo>,
    /// Struct types that are not registered themselves but appear
    /// (transitively) as property types of registered ones. Sorted by
    /// name.
    pub referenced_structs: Vec<TypeInfo>,
    /// Enum types appearing (transitively) as property types of any
    /// listed type. Sorted by name.
    pub referenced_enums: Vec<EnumInfo>,
    /// Whether the respective linkme section exists at all. Both
    /// `false` usually means "not built against dots-rs" (a section
    /// with zero entries can also be absent entirely — the linker
    /// drops empty sections).
    pub has_published_section: bool,
    pub has_subscribed_section: bool,
    pub has_subscribed_filtered_section: bool,
}

#[derive(Debug)]
pub enum InspectError {
    /// The file could not be parsed as an object file.
    Object(object::Error),
    /// Parsed, but not something this tool can handle (not ELF, not
    /// 64-bit, ...).
    Unsupported(String),
    /// The linkme sections or descriptors don't look like dots-rs
    /// output (layout mismatch, dangling pointer, bad UTF-8, ...).
    Malformed(String),
}

impl std::fmt::Display for InspectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Object(e) => write!(f, "failed to parse object file: {e}"),
            Self::Unsupported(msg) => write!(f, "unsupported binary: {msg}"),
            Self::Malformed(msg) => write!(f, "malformed dots-rs metadata: {msg}"),
        }
    }
}

impl std::error::Error for InspectError {}

impl From<object::Error> for InspectError {
    fn from(e: object::Error) -> Self {
        Self::Object(e)
    }
}

// Offsets and strides inside the inspected binary, taken from the
// host-side descriptor types: `repr(C)` plus the all-64-bit rule above
// makes host and target layouts identical.
const STRUCT_NAME: u64 = offset_of!(StructDescriptor, name) as u64;
const STRUCT_FLAGS: u64 = offset_of!(StructDescriptor, flags) as u64;
const STRUCT_SIZE: u64 = offset_of!(StructDescriptor, size) as u64;
const STRUCT_ALIGN: u64 = offset_of!(StructDescriptor, align) as u64;
const STRUCT_PROPERTIES: u64 = offset_of!(StructDescriptor, properties) as u64;

const PROPERTY_NAME: u64 = offset_of!(PropertyDescriptor, name) as u64;
const PROPERTY_TAG: u64 = offset_of!(PropertyDescriptor, tag) as u64;
const PROPERTY_IS_KEY: u64 = offset_of!(PropertyDescriptor, is_key) as u64;
const PROPERTY_KIND: u64 = offset_of!(PropertyDescriptor, kind) as u64;
const PROPERTY_STRIDE: u64 = size_of::<PropertyDescriptor>() as u64;

const ENUM_NAME: u64 = offset_of!(EnumDescriptor, name) as u64;
const ENUM_ELEMENTS: u64 = offset_of!(EnumDescriptor, elements) as u64;
const ELEMENT_NAME: u64 = offset_of!(EnumElement, name) as u64;
const ELEMENT_TAG: u64 = offset_of!(EnumElement, tag) as u64;
const ELEMENT_VALUE: u64 = offset_of!(EnumElement, value) as u64;
const ELEMENT_STRIDE: u64 = size_of::<EnumElement>() as u64;

// `FieldKind` is `repr(C, u8)`: discriminant byte at offset 0, variant
// payload (a pointer, where present) at offset 8. The discriminant
// values are declared explicitly in dots-rs-core and are append-only.
const KIND_PAYLOAD: u64 = 8;
const KIND_VEC: u8 = 16;
const KIND_STRUCT: u8 = 17;
const KIND_ENUM: u8 = 18;

/// `.dots` IDL spelling of a payload-free `FieldKind` discriminant
/// (the names dots-rs-build's parser accepts).
fn scalar_idl_name(discriminant: u8) -> Option<&'static str> {
    Some(match discriminant {
        0 => "bool",
        1 => "uint8",
        2 => "uint16",
        3 => "uint32",
        4 => "uint64",
        5 => "int8",
        6 => "int16",
        7 => "int32",
        8 => "int64",
        9 => "float32",
        10 => "float64",
        11 => "property_set",
        12 => "timepoint",
        13 => "duration",
        14 => "string",
        15 => "uuid",
        19 => "any",
        _ => return None,
    })
}

// Fat pointers (`&str`, `&[T]`) are (data pointer, length) — the
// de-facto layout noted in the crate docs.
const FAT_LEN_OFFSET: u64 = 8;

/// Longest name and largest array we accept before concluding the
/// layout assumptions are wrong for this binary.
const MAX_NAME_LEN: u64 = 4096;
const MAX_LIST_LEN: u64 = 4096;
/// Deepest `vector<vector<...>>` nesting accepted.
const MAX_KIND_DEPTH: u32 = 32;

/// A readable view of the binary's virtual address space: the loadable
/// segments plus the dynamic RELATIVE relocations that PIE binaries
/// rely on to fill pointer slots at load time.
struct Image<'d> {
    little_endian: bool,
    /// (virtual address, file bytes) per loadable segment.
    segments: Vec<(u64, &'d [u8])>,
    /// Pointer-slot vaddr → relocated value (link-time base 0).
    relocs: HashMap<u64, u64>,
}

impl<'d> Image<'d> {
    fn bytes(&self, addr: u64, len: u64) -> Result<&'d [u8], InspectError> {
        for &(base, data) in &self.segments {
            if addr >= base && addr - base < data.len() as u64 {
                let offset = (addr - base) as usize;
                let end = offset.saturating_add(len as usize);
                if end <= data.len() {
                    return Ok(&data[offset..end]);
                }
            }
        }
        Err(InspectError::Malformed(format!(
            "address {addr:#x}..+{len} is not backed by file data"
        )))
    }

    fn byte(&self, addr: u64) -> Result<u8, InspectError> {
        Ok(self.bytes(addr, 1)?[0])
    }

    fn u32(&self, addr: u64) -> Result<u32, InspectError> {
        let bytes: [u8; 4] = self.bytes(addr, 4)?.try_into().expect("length checked");
        Ok(if self.little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn word(&self, addr: u64) -> Result<u64, InspectError> {
        let bytes: [u8; 8] = self.bytes(addr, 8)?.try_into().expect("length checked");
        Ok(if self.little_endian {
            u64::from_le_bytes(bytes)
        } else {
            u64::from_be_bytes(bytes)
        })
    }

    /// Read a pointer-sized slot: a dynamic relocation wins over the
    /// bytes at rest (which are typically zero in a PIE binary).
    fn pointer(&self, addr: u64) -> Result<u64, InspectError> {
        match self.relocs.get(&addr) {
            Some(&value) => Ok(value),
            None => self.word(addr),
        }
    }

    /// Read a `&str`-typed field (fat pointer at `addr`).
    fn str(&self, addr: u64, what: &str) -> Result<String, InspectError> {
        let ptr = self.pointer(addr)?;
        let len = self.word(addr + FAT_LEN_OFFSET)?;
        if len > MAX_NAME_LEN {
            return Err(InspectError::Malformed(format!(
                "{what} at {addr:#x}: string length {len} is implausible — \
                 binary built against an incompatible dots-rs version?"
            )));
        }
        Ok(std::str::from_utf8(self.bytes(ptr, len)?)
            .map_err(|e| {
                InspectError::Malformed(format!("{what} at {addr:#x}: not UTF-8: {e}"))
            })?
            .to_owned())
    }
}

/// The `R_*_RELATIVE` relocation type for the given architecture.
fn relative_reloc_type(arch: Architecture) -> Option<object::elf::RelocationType> {
    use object::elf;
    Some(match arch {
        Architecture::X86_64 => elf::R_X86_64_RELATIVE,
        Architecture::Aarch64 => elf::R_AARCH64_RELATIVE,
        Architecture::Riscv64 => elf::R_RISCV_RELATIVE,
        Architecture::LoongArch64 => elf::R_LARCH_RELATIVE,
        Architecture::PowerPc64 => elf::R_PPC64_RELATIVE,
        Architecture::S390x => elf::R_390_RELATIVE,
        _ => return None,
    })
}

/// Inspect the raw contents of an ELF binary built against dots-rs.
pub fn inspect(data: &[u8]) -> Result<Report, InspectError> {
    let obj = object::File::parse(data)?;

    if obj.format() != BinaryFormat::Elf {
        return Err(InspectError::Unsupported(format!(
            "{:?} — dots-rs registration sections only exist on ELF targets",
            obj.format()
        )));
    }
    if !obj.is_64() {
        return Err(InspectError::Unsupported(
            "32-bit ELF — only 64-bit binaries are supported".into(),
        ));
    }

    let mut segments = Vec::new();
    for segment in obj.segments() {
        segments.push((segment.address(), segment.data()?));
    }

    // Dynamic RELATIVE relocations. A non-PIE executable has none and
    // stores real addresses at rest, so an empty map is fine. On an
    // architecture we don't know the RELATIVE code for, a PIE binary
    // would fail later with a clear "not backed by file data" error.
    let relative = relative_reloc_type(obj.architecture());
    let mut relocs = HashMap::new();
    if let (Some(dynamic), Some(relative)) = (obj.dynamic_relocations(), relative) {
        for (addr, reloc) in dynamic {
            match reloc.flags() {
                RelocationFlags::Elf { r_type } if r_type == relative => {
                    relocs.insert(addr, reloc.addend() as u64);
                }
                _ => {}
            }
        }
    }

    let image = Image {
        little_endian: obj.is_little_endian(),
        segments,
        relocs,
    };

    let mut report = Report::default();
    let mut published_addrs = Vec::new();
    let mut subscribed_addrs = Vec::new();
    let mut subscribed_filtered_addrs = Vec::new();
    if let Some(section) = obj.section_by_name(PUBLISHED_SECTION) {
        report.has_published_section = true;
        published_addrs = read_slice(&image, section.address(), section.size())?;
    }
    if let Some(section) = obj.section_by_name(SUBSCRIBED_SECTION) {
        report.has_subscribed_section = true;
        subscribed_addrs = read_slice(&image, section.address(), section.size())?;
    }
    if let Some(section) = obj.section_by_name(SUBSCRIBED_FILTERED_SECTION) {
        report.has_subscribed_filtered_section = true;
        subscribed_filtered_addrs = read_slice(&image, section.address(), section.size())?;
    }

    let mut parser = Parser {
        image: &image,
        structs: HashMap::new(),
        enums: HashMap::new(),
        pending_structs: Vec::new(),
        pending_enums: Vec::new(),
    };
    for &addr in published_addrs
        .iter()
        .chain(&subscribed_addrs)
        .chain(&subscribed_filtered_addrs)
    {
        parser.parse_struct(addr)?;
    }
    // Property types of registered structs pull in further struct and
    // enum descriptors; drain until the transitive closure is parsed.
    while let Some(addr) = parser.pending_structs.pop() {
        parser.parse_struct(addr)?;
    }
    while let Some(addr) = parser.pending_enums.pop() {
        parser.parse_enum(addr)?;
    }

    let registered: HashSet<u64> = published_addrs
        .iter()
        .chain(&subscribed_addrs)
        .chain(&subscribed_filtered_addrs)
        .copied()
        .collect();
    report.published = collect(&parser.structs, &published_addrs);
    report.subscribed = collect(&parser.structs, &subscribed_addrs);
    report.subscribed_filtered = collect(&parser.structs, &subscribed_filtered_addrs);
    report.referenced_structs = parser
        .structs
        .values()
        .filter(|t| !registered.contains(&t.descriptor_addr))
        .cloned()
        .collect();
    report.referenced_structs.sort_by(|a, b| a.name.cmp(&b.name));
    report.referenced_enums = parser.enums.into_values().collect();
    report.referenced_enums.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report)
}

fn collect(structs: &HashMap<u64, TypeInfo>, addrs: &[u64]) -> Vec<TypeInfo> {
    let mut types: Vec<TypeInfo> = addrs
        .iter()
        .map(|addr| structs[addr].clone())
        .collect();
    types.sort_by(|a, b| a.name.cmp(&b.name));
    types
}

/// Read one linkme section — an array of `&'static StructDescriptor` —
/// returning the deduplicated descriptor addresses.
fn read_slice(image: &Image<'_>, addr: u64, size: u64) -> Result<Vec<u64>, InspectError> {
    if size % 8 != 0 {
        return Err(InspectError::Malformed(format!(
            "linkme section size {size} is not a multiple of the pointer size"
        )));
    }
    let mut seen = HashSet::new();
    let mut addrs = Vec::new();
    for slot in (addr..addr + size).step_by(8) {
        let descriptor = image.pointer(slot)?;
        if seen.insert(descriptor) {
            addrs.push(descriptor);
        }
    }
    Ok(addrs)
}

struct Parser<'a, 'd> {
    image: &'a Image<'d>,
    structs: HashMap<u64, TypeInfo>,
    enums: HashMap<u64, EnumInfo>,
    pending_structs: Vec<u64>,
    pending_enums: Vec<u64>,
}

impl Parser<'_, '_> {
    fn parse_struct(&mut self, addr: u64) -> Result<(), InspectError> {
        if self.structs.contains_key(&addr) {
            return Ok(());
        }
        let image = self.image;
        let name = image.str(addr + STRUCT_NAME, "struct descriptor name")?;
        let flags = StructFlags::from_bits(image.byte(addr + STRUCT_FLAGS)?);
        let size = image.word(addr + STRUCT_SIZE)?;
        let align = image.word(addr + STRUCT_ALIGN)?;
        let properties_ptr = image.pointer(addr + STRUCT_PROPERTIES)?;
        let properties_len = image.word(addr + STRUCT_PROPERTIES + FAT_LEN_OFFSET)?;

        // Plausibility checks that catch descriptors laid out
        // differently than the host-side `StructDescriptor` — most
        // likely a binary built against a dots-rs predating the
        // `repr(C)` descriptor layout, whose compiler-chosen field
        // order shuffles what we just read. The name tends to survive
        // (it sits at offset 0 either way), so without these checks
        // the mismatch shows up as subtly wrong output instead of an
        // error.
        let plausible = align.is_power_of_two()
            && align <= 4096
            && size % align == 0
            && properties_len <= MAX_LIST_LEN
            && flags.bits() & !StructFlags::ALL_KNOWN.bits() == 0;
        if !plausible {
            return Err(InspectError::Malformed(format!(
                "descriptor for \"{name}\" at {addr:#x} doesn't match the expected \
                 repr(C) layout — was the binary built against an older dots-rs \
                 whose StructDescriptor wasn't repr(C) yet?"
            )));
        }

        let mut properties = Vec::with_capacity(properties_len as usize);
        for i in 0..properties_len {
            properties.push(self.parse_property(properties_ptr + i * PROPERTY_STRIDE)?);
        }
        self.structs.insert(
            addr,
            TypeInfo {
                name,
                flags,
                size,
                align,
                properties,
                descriptor_addr: addr,
            },
        );
        Ok(())
    }

    fn parse_property(&mut self, addr: u64) -> Result<PropertyInfo, InspectError> {
        let image = self.image;
        Ok(PropertyInfo {
            name: image.str(addr + PROPERTY_NAME, "property name")?,
            tag: image.u32(addr + PROPERTY_TAG)?,
            is_key: image.byte(addr + PROPERTY_IS_KEY)? != 0,
            kind: self.parse_kind(addr + PROPERTY_KIND, 0)?,
        })
    }

    /// Parse a `FieldKind` at `addr` (inline in a property descriptor,
    /// or behind a `Vec` payload pointer). Struct/enum payloads only
    /// have their name read here; the full descriptor is queued for
    /// the worklist, which also keeps reference cycles harmless.
    fn parse_kind(&mut self, addr: u64, depth: u32) -> Result<KindInfo, InspectError> {
        if depth > MAX_KIND_DEPTH {
            return Err(InspectError::Malformed(format!(
                "field kind at {addr:#x}: vector nesting deeper than {MAX_KIND_DEPTH}"
            )));
        }
        let discriminant = self.image.byte(addr)?;
        if let Some(name) = scalar_idl_name(discriminant) {
            return Ok(KindInfo::Scalar(name));
        }
        let payload = self.image.pointer(addr + KIND_PAYLOAD)?;
        match discriminant {
            KIND_VEC => Ok(KindInfo::Vec(Box::new(
                self.parse_kind(payload, depth + 1)?,
            ))),
            KIND_STRUCT => {
                let name = self.image.str(payload + STRUCT_NAME, "struct descriptor name")?;
                if !self.structs.contains_key(&payload) {
                    self.pending_structs.push(payload);
                }
                Ok(KindInfo::Struct(name))
            }
            KIND_ENUM => {
                let name = self.image.str(payload + ENUM_NAME, "enum descriptor name")?;
                if !self.enums.contains_key(&payload) {
                    self.pending_enums.push(payload);
                }
                Ok(KindInfo::Enum(name))
            }
            other => Err(InspectError::Malformed(format!(
                "field kind at {addr:#x}: unknown discriminant {other} — binary \
                 built against a newer dots-rs than this dots-inspect?"
            ))),
        }
    }

    fn parse_enum(&mut self, addr: u64) -> Result<(), InspectError> {
        if self.enums.contains_key(&addr) {
            return Ok(());
        }
        let image = self.image;
        let name = image.str(addr + ENUM_NAME, "enum descriptor name")?;
        let elements_ptr = image.pointer(addr + ENUM_ELEMENTS)?;
        let elements_len = image.word(addr + ENUM_ELEMENTS + FAT_LEN_OFFSET)?;
        if elements_len > MAX_LIST_LEN {
            return Err(InspectError::Malformed(format!(
                "enum \"{name}\" at {addr:#x}: {elements_len} elements is implausible"
            )));
        }
        let mut elements = Vec::with_capacity(elements_len as usize);
        for i in 0..elements_len {
            let element = elements_ptr + i * ELEMENT_STRIDE;
            elements.push(EnumElementInfo {
                name: image.str(element + ELEMENT_NAME, "enum element name")?,
                tag: image.u32(element + ELEMENT_TAG)?,
                value: image.u32(element + ELEMENT_VALUE)? as i32,
            });
        }
        self.enums.insert(
            addr,
            EnumInfo {
                name,
                elements,
                descriptor_addr: addr,
            },
        );
        Ok(())
    }
}

impl TypeInfo {
    /// Render the type as `.dots` IDL source (the syntax
    /// `dots-rs-build` compiles).
    pub fn to_idl(&self) -> String {
        let mut out = String::new();
        let _ = write!(out, "struct {}", self.name);
        let flags = flag_names(self.flags);
        if !flags.is_empty() {
            let _ = write!(out, " [{}]", flags.join(", "));
        }
        out.push_str(" {\n");
        for p in &self.properties {
            let key = if p.is_key { "[key] " } else { "" };
            let _ = writeln!(out, "    {}: {key}{} {};", p.tag, p.kind, p.name);
        }
        out.push_str("}\n");
        out
    }
}

impl EnumInfo {
    /// Render the enum as `.dots` IDL source.
    pub fn to_idl(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "enum {} {{", self.name);
        for (i, e) in self.elements.iter().enumerate() {
            let _ = write!(out, "    {}: {}", e.tag, e.name);
            // `value` is descriptor metadata defaulting to `tag - 1`;
            // spell it out only when it deviates, like `.dots` sources do.
            if i64::from(e.value) != i64::from(e.tag) - 1 {
                let _ = write!(out, " = {}", e.value);
            }
            out.push_str(if i + 1 < self.elements.len() { ",\n" } else { "\n" });
        }
        out.push_str("}\n");
        out
    }
}

fn flag_names(flags: StructFlags) -> Vec<&'static str> {
    let mut names = Vec::new();
    if flags.is_cached() {
        names.push("cached");
    }
    if flags.is_internal() {
        names.push("internal");
    }
    if flags.is_persistent() {
        names.push("persistent");
    }
    if flags.is_cleanup() {
        names.push("cleanup");
    }
    if flags.is_local() {
        names.push("local");
    }
    if flags.is_substruct_only() {
        names.push("substruct_only");
    }
    names
}
