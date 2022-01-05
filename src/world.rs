use std::collections::HashMap;
use std::{io, mem};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::sync::Arc;
use byteorder::{BigEndian, ReadBytesExt};
use dashmap::mapref::entry::Entry;
use internment::ArcIntern;
use num_integer::Integer;
use positioned_io_preview::{RandomAccessFile, ReadAt};
use crate::fname::{CommonFNames, FName};
use crate::util::{FastDashMap, make_fast_dash_map};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkPos {
    x: i32,
    z: i32,
}

impl ChunkPos {
    pub fn new(x: i32, z: i32) -> Self {
        ChunkPos { x, z }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BlockPos {
    x: i32,
    y: i32,
    z: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockState {
    block: FName,
    properties: HashMap<FName, FName>,
}

#[allow(clippy::derive_hash_xor_eq)]
impl Hash for BlockState {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.block.hash(state);
        let mut result = 0_u64;
        for (k, v) in self.properties.iter() {
            let mut hasher = DefaultHasher::new();
            k.hash(&mut hasher);
            v.hash(&mut hasher);
            result ^= hasher.finish();
        }
        result.hash(state);
    }
}

pub type IBlockState = ArcIntern<BlockState>;

impl BlockState {
    pub fn new(block: FName) -> Self {
        BlockState {
            block,
            properties: HashMap::new(),
        }
    }
}

macro_rules! define_paletted_data {
    ($name:ident, $type:ty, $h_bits:expr, $v_bits:expr, $default_palette_size:expr) => {
        struct $name {
            entries_per_long: u8,
            bits_per_block: u8,
            palette: Vec<$type>,
            inv_palette: HashMap<$type, usize>,
            data: Vec<u64>,
        }

        impl $name {
            fn new() -> Self {
                let bits_per_block = $default_palette_size.log2() as u8;
                let entries_per_long = 64_u8 / bits_per_block;
                $name {
                    bits_per_block,
                    entries_per_long,
                    palette: Vec::with_capacity($default_palette_size),
                    inv_palette: HashMap::new(),
                    data: Vec::with_capacity(((1_usize << $h_bits) * (1_usize << $h_bits) * (1_usize << $v_bits)).div_ceil(entries_per_long as usize)),
                }
            }

            fn direct_init(palette: Vec<$type>, data: Vec<u64>) -> Self {
                let bits_per_block = palette.len().log2() as u8;
                let entries_per_long = 64_u8 / bits_per_block;
                let mut inv_palette = HashMap::new();
                for (i, v) in palette.iter().enumerate() {
                    inv_palette.insert(v.clone(), i);
                }
                $name {
                    bits_per_block,
                    entries_per_long,
                    palette,
                    inv_palette,
                    data,
                }
            }

            fn get(&self, x: usize, y: usize, z: usize) -> &$type {
                let index = x << ($h_bits + $v_bits) | z << $v_bits | y;
                let (bit, inbit) = index.div_mod_floor(&(self.entries_per_long as usize));
                return &self.palette[((self.data[bit] >> (inbit * self.bits_per_block as usize)) & ((1 << self.bits_per_block) - 1)) as usize];
            }

            fn set(&mut self, x: usize, y: usize, z: usize, value: &$type) {
                let val = match self.inv_palette.get(value) {
                    Some(val) => *val,
                    None => {
                        if self.palette.len() == 1 << self.bits_per_block {
                            self.resize();
                        }
                        self.palette.push(value.clone());
                        self.inv_palette.insert(value.clone(), self.palette.len() - 1);
                        self.palette.len() - 1
                    }
                };
                let index = x << ($h_bits + $v_bits) | z << $v_bits | y;
                let (bit, inbit) = index.div_mod_floor(&(self.entries_per_long as usize));
                self.data[bit] &= !(((1 << self.bits_per_block) - 1) << (inbit * self.bits_per_block as usize));
                self.data[bit] |= (val as u64) << (inbit * self.bits_per_block as usize);
            }

            fn resize(&mut self) {
                let old_data_size = ((1 << $h_bits) * (1 << $h_bits) * (1 << $v_bits)).div_ceil(&(self.entries_per_long as usize));
                let old_bits_per_block = self.bits_per_block;
                let old_entries_per_long = self.entries_per_long;
                self.palette.reserve(self.palette.len());
                self.bits_per_block += 1;
                self.entries_per_long = 64_u8.div_floor(self.bits_per_block);
                let old_data = mem::replace(&mut self.data, Vec::with_capacity(((1 << $h_bits) * (1 << $h_bits) * (1 << $v_bits)).div_ceil(&(self.entries_per_long as usize))));
                let mut block = 0;
                for index in 0..old_data_size - 1 {
                    let word = old_data[index];
                    for i in 0..old_entries_per_long {
                        let entry = (word >> (i * old_bits_per_block)) & ((1 << old_bits_per_block) - 1);
                        block = (block << self.bits_per_block) | entry;
                        if (index * old_entries_per_long as usize + i as usize + 1) % self.entries_per_long as usize == 0 {
                            self.data.push(block);
                            block = 0;
                        }
                    }
                }
                if ((1_u64 << $h_bits) * (1_u64 << $h_bits) * (1_u64 << $v_bits)) % self.entries_per_long as u64 != 0 {
                    self.data.push(block);
                }
            }
        }
    };
}

define_paletted_data!(BlockData, IBlockState, 4_usize, 4_usize, 16_usize);
define_paletted_data!(BiomeData, FName, 2_usize, 2_usize, 4_usize);

pub struct Subchunk {
    block_data: BlockData,
    biome_data: BiomeData,
}

pub struct Chunk {
    pub subchunks: Vec<Option<Subchunk>>,
}

impl Chunk {
    pub fn empty() -> Self {
        Chunk {
            subchunks: Vec::new(),
        }
    }
}

pub struct Dimension {
    id: FName,
    min_y: i32,
    max_y: i32,
    chunks: FastDashMap<ChunkPos, Arc<Chunk>>,
}

impl Dimension {
    pub fn new(id: FName) -> Self {
        Dimension {
            id,
            min_y: 0,
            max_y: 256,
            chunks: make_fast_dash_map(),
        }
    }

    fn get_save_dir(&self, world: &World) -> PathBuf {
        if self.id == CommonFNames.OVERWORLD {
            world.path.clone()
        } else if self.id == CommonFNames.THE_NETHER {
            world.path.join("DIM-1")
        } else if self.id == CommonFNames.THE_END {
            world.path.join("DIM1")
        } else {
            world.path.join("dimensions").join(self.id.namespace.clone()).join(self.id.name.clone())
        }
    }

    pub fn get_chunk(&self, pos: ChunkPos) -> Option<Arc<Chunk>> {
        match &self.chunks.entry(pos) {
            Entry::Occupied(entry) => Some(entry.get().clone()),
            Entry::Vacant(_) => None
        }
    }

    pub fn load_chunk(&self, world: &World, pos: ChunkPos) {
        let _chunk = self.read_chunk(world, pos).unwrap();
    }

    fn read_chunk(&self, world: &World, pos: ChunkPos) -> io::Result<Chunk> {
        let save_dir = self.get_save_dir(world);
        let region_path = save_dir.join("region").join(format!("r.{}.{}.mca", pos.x >> 5, pos.z >> 5));
        let raf = RandomAccessFile::open(region_path)?;
        #[allow(clippy::uninit_assumed_init)]
        let mut sector_data: [u8; 4] = unsafe { MaybeUninit::uninit().assume_init() };
        raf.read_exact_at((((pos.x & 31) | ((pos.z & 31) << 5)) << 2) as u64, &mut sector_data)?;
        let offset = Cursor::new(sector_data).read_u24::<BigEndian>()? as u64 * 4096;
        let size = sector_data[3] as usize * 4096;
        if size < 5 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Chunk header is truncated"));
        }
        let mut buffer = Vec::with_capacity(size);
        #[allow(clippy::uninit_vec)]
        unsafe { buffer.set_len(size); }
        raf.read_exact_at(offset, &mut buffer)?;
        let mut cursor = Cursor::new(&buffer);
        let m = cursor.read_i32::<BigEndian>()?;
        let b = cursor.read_u8()?;
        if m == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Chunk is allocated, but stream is missing"));
        }
        if b & 128 != 0 {
            if m != 1 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Chunk has both internal and external streams"));
            }
            // TODO: read external chunks
            return Err(io::Error::new(io::ErrorKind::InvalidData, "External chunk"));
        }
        if m < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Declared size {} of chunk is negative", m)));
        }
        let n = (m - 1) as usize;
        if n > size - 5 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Declared size {} of chunk is larger than actual size {}", n, size)));
        }
        let data: nbt::Blob = match b {
            1 => nbt::from_gzip_reader(cursor)?,
            2 => nbt::from_zlib_reader(cursor)?,
            3 => nbt::from_reader(cursor)?,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown compression type")),
        };

        let mut chunk = Chunk::empty();

        if let Some(nbt::Value::List(sections)) = data.get("sections") {
            'sections:
            for section in sections.iter().take(((self.max_y - self.min_y) / 16) as usize) {
                if let nbt::Value::Compound(section_map) = section {
                    let mut data = Vec::new();
                    let mut palette = Vec::new();
                    if let Some(nbt::Value::Compound(block_states)) = section_map.get("block_states") {
                        if let Some(nbt::Value::List(nbt_data)) = block_states.get("data") {
                            data.reserve(nbt_data.len());
                            for data_elem in nbt_data {
                                match data_elem {
                                    nbt::Value::Byte(b) => data.push(*b as u8 as u64),
                                    nbt::Value::Short(s) => data.push(*s as u16 as u64),
                                    nbt::Value::Int(i) => data.push(*i as u32 as u64),
                                    nbt::Value::Long(l) => data.push(*l as u64),
                                    _ => {
                                        chunk.subchunks.push(None);
                                        continue 'sections
                                    }
                                }
                            }
                        } else {
                            chunk.subchunks.push(None);
                            continue 'sections
                        }
                        if let Some(nbt::Value::List(palette_list)) = block_states.get("palette") {
                            palette.reserve(palette_list.len());
                            for palette_elem in palette_list {
                                match palette_elem {
                                    nbt::Value::Compound(palette_map) => {
                                        if let Some(nbt::Value::String(name)) = palette_map.get("Name") {
                                            let mut block_state = BlockState::new(FName::new(name.parse().unwrap()));
                                            if let Some(nbt::Value::Compound(properties)) = palette_map.get("Properties") {
                                                let mut valid = true;
                                                block_state.properties = properties.iter().map(|(k, v)| (FName::new(k.parse().unwrap()), FName::new(match v {
                                                    nbt::Value::String(s) => s.to_owned(),
                                                    nbt::Value::Byte(b) => b.to_owned().to_string(),
                                                    nbt::Value::Short(s) => s.to_owned().to_string(),
                                                    nbt::Value::Int(i) => i.to_owned().to_string(),
                                                    nbt::Value::Long(l) => l.to_owned().to_string(),
                                                    nbt::Value::Float(f) => f.to_owned().to_string(),
                                                    nbt::Value::Double(d) => d.to_owned().to_string(),
                                                    _ => {
                                                        valid = false;
                                                        CommonFNames.AIR.to_string()
                                                    }
                                                }.parse().unwrap()))).collect();
                                                if !valid {
                                                    chunk.subchunks.push(None);
                                                    continue 'sections
                                                }
                                            }
                                            palette.push(IBlockState::new(block_state));
                                        } else {
                                            chunk.subchunks.push(None);
                                            continue 'sections
                                        }
                                    }
                                    _ => {
                                        chunk.subchunks.push(None);
                                        continue 'sections
                                    }
                                }
                            }
                        } else {
                            chunk.subchunks.push(None);
                            continue 'sections
                        }
                    } else {
                        chunk.subchunks.push(None);
                        continue 'sections
                    }

                    chunk.subchunks.push(Some(Subchunk {
                        block_data: BlockData::direct_init(palette, data),
                        biome_data: BiomeData::new(),
                    }));
                } else {
                    chunk.subchunks.push(None);
                    continue 'sections
                }
            }
        }

        println!("{}", data);

        return Ok(chunk);
    }
}

pub struct World {
    path: PathBuf,
    dimensions: FastDashMap<FName, Arc<Dimension>>,
}

impl World {
    pub fn new(path: PathBuf) -> World {
        let world = World {
            path,
            dimensions: make_fast_dash_map()
        };
        let mut overworld = Dimension::new(CommonFNames.OVERWORLD.clone());
        overworld.min_y = -64;
        overworld.max_y = 384;
        world.dimensions.insert(CommonFNames.OVERWORLD.clone(), Arc::new(overworld));
        world.dimensions.insert(CommonFNames.THE_NETHER.clone(), Arc::new(Dimension::new(CommonFNames.THE_NETHER.clone())));
        world.dimensions.insert(CommonFNames.THE_END.clone(), Arc::new(Dimension::new(CommonFNames.THE_END.clone())));
        world
    }

    pub fn get_dimension(&self, id: FName) -> Option<Arc<Dimension>> {
        match self.dimensions.entry(id) {
            Entry::Occupied(entry) => Some(entry.get().clone()),
            Entry::Vacant(_) => None
        }
    }
}