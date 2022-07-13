use std::{
    collections::HashMap,
    io::{Seek, SeekFrom, Write},
};

use bytes::Buf;
use models::{FieldId, Timestamp, ValueType};
use snafu::ResultExt;
use utils::{BkdrHasher, BloomFilter};

use super::{
    block, index::Index, BlockMetaIterator, BLOCK_META_SIZE, BLOOM_FILTER_BITS, INDEX_META_SIZE,
    MAX_BLOCK_VALUES,
};
use crate::{
    direct_io::{FileCursor, FileSync},
    error::{self, Error, Result},
    tsm::{BlockMeta, DataBlock},
};

// A TSM file is composed for four sections: header, blocks, index and the footer.
//
// ┌────────┬────────────────────────────────────┬─────────────┬──────────────┐
// │ Header │               Blocks               │    Index    │    Footer    │
// │5 bytes │              N bytes               │   N bytes   │   8 bytes    │
// └────────┴────────────────────────────────────┴─────────────┴──────────────┘
//
// ┌───────────────────┐
// │      Header       │
// ├─────────┬─────────┤
// │  Magic  │ Version │
// │ 4 bytes │ 1 byte  │
// └─────────┴─────────┘
//
// ┌───────────────────────────────────────┐
// │               Blocks                  │
// ├───────────────────┬───────────────────┤
// │                Block                  │
// ├─────────┬─────────┼─────────┬─────────┼
// │  CRC    │ ts      │  CRC    │  value  │
// │ 4 bytes │ N bytes │ 4 bytes │ N bytes │
// └─────────┴─────────┴─────────┴─────────┴
//
// ┌──────────────────────────────────────────────────────────────────────┐
// │                               Index                                  │
// ├─────────┬──────┬───────┬─────────┬─────────┬────────┬────────┬───────┤
// │ fieldId │ Type │ Count │Min Time │Max Time │ Offset │  Size  │Valoff │
// │ 8 bytes │1 byte│2 bytes│ 8 bytes │ 8 bytes │8 bytes │8 bytes │8 bytes│
// └─────────┴──────┴───────┴─────────┴─────────┴────────┴────────┴───────┘
//
// ┌─────────────────────────┐
// │ Footer                  │
// ├───────────────┬─────────┤
// │ Bloom Filter  │Index Ofs│
// │ 8 bytes       │ 8 bytes │
// └───────────────┴─────────┘

const HEADER_LEN: u64 = 5;
const TSM_MAGIC: u32 = 0x1346613;
const VERSION: u8 = 1;

pub trait TsmWriter {
    fn write_header(&mut self) -> Result<usize>;
    fn write_blocks(&mut self) -> Result<usize>;
    fn write_index(&mut self) -> Result<usize>;
    fn write_footer(&mut self) -> Result<usize>;
    fn flush(&mut self) -> Result<()>;

    fn write(&mut self) -> Result<usize> {
        let mut size = 0_usize;
        self.write_header()
            .and_then(|i| {
                size += i;
                self.write_blocks()
            })
            .and_then(|i| {
                size += i;
                self.write_index()
            })
            .and_then(|i| {
                size += i;
                self.write_footer()
            })
            .and_then(|size| self.flush())
            .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;

        Ok(size)
    }
}

struct IndexBuf {
    index_offset: u64,
    index_meta: Vec<u8>,
    last_block_meta_offset: usize,
    block_meta_offsets: Vec<usize>,
    block_meta: Vec<u8>,

    bloom_filter: BloomFilter,
}

impl IndexBuf {
    pub fn new() -> Self {
        Self { index_offset: 0,
               index_meta: Vec::new(),
               last_block_meta_offset: 0,
               block_meta_offsets: Vec::new(),
               block_meta: Vec::new(),
               bloom_filter: BloomFilter::new(BLOOM_FILTER_BITS) }
    }

    pub fn set_index_offset(&mut self, index_offset: u64) {
        self.index_offset = index_offset;
    }

    pub fn insert_index_meta(&mut self,
                             field_id: FieldId,
                             block_type: ValueType,
                             block_count: u16) {
        self.index_meta.extend_from_slice(&field_id.to_be_bytes()[..]);
        self.index_meta.extend_from_slice(&[u8::from(block_type)][..]);
        self.index_meta.extend_from_slice(&block_count.to_be_bytes()[..]);
        self.block_meta_offsets.push(self.last_block_meta_offset);
        self.last_block_meta_offset = self.block_meta.len();
    }

    pub fn insert_block_meta(&mut self,
                             min_ts: i64,
                             max_ts: i64,
                             offset: u64,
                             size: u64,
                             val_off: u64) {
        self.block_meta.extend_from_slice(&min_ts.to_be_bytes()[..]);
        self.block_meta.extend_from_slice(&max_ts.to_be_bytes()[..]);
        self.block_meta.extend_from_slice(&offset.to_be_bytes()[..]);
        self.block_meta.extend_from_slice(&size.to_be_bytes()[..]);
        self.block_meta.extend_from_slice(&val_off.to_be_bytes()[..]);
    }

    pub fn write_to(&self, writer: &mut FileCursor) -> Result<usize> {
        let mut size = 0_usize;
        let mut index_pos = 0_usize;
        let mut index_idx = 0_usize;
        while index_pos < self.index_meta.len() {
            writer.write(&self.index_meta[index_pos..index_pos + INDEX_META_SIZE])
                  .map(|s| size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;
            index_pos += INDEX_META_SIZE;
            let blocks_sli = match self.block_meta_offsets.get(index_idx + 1) {
                Some(nbp) => &self.block_meta[self.block_meta_offsets[index_idx]..*nbp],
                None => &self.block_meta[self.block_meta_offsets[index_idx]..],
            };
            writer.write(&blocks_sli)
                  .map(|s| size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;
            index_idx += 1;
        }

        Ok(size)
    }
}

pub struct TsmCacheWriter {
    writer: FileCursor,
    cached_blocks: HashMap<FieldId, DataBlock>,
    index_buf: IndexBuf,
}

impl TsmCacheWriter {
    pub fn new(writer: FileCursor, blocks: HashMap<FieldId, DataBlock>) -> Self {
        Self { writer, cached_blocks: blocks, index_buf: IndexBuf::new() }
    }

    fn write_one_block(writer: &mut FileCursor,
                       index_buf: &mut IndexBuf,
                       field_id: FieldId,
                       block: &DataBlock)
                       -> Result<usize> {
        let point_cnt = block.len();
        let block_count = ((point_cnt - 1) / MAX_BLOCK_VALUES + 1) as u16;
        let idx_meta_beg = writer.pos();
        let block_type = block.field_type();
        let mut min_ts: i64;
        let mut max_ts: i64;
        let mut offset: u64;
        let mut val_off: u64;

        let ts_sli = block.ts();

        let field_type = block.field_type();
        let mut i = 0_usize;
        let mut last_index = 0_usize;
        let mut total_size = 0_usize;
        let mut blk_size: usize;
        while i < block_count as usize {
            blk_size = 0_usize;
            let start = last_index;
            let end = point_cnt % MAX_BLOCK_VALUES + i * MAX_BLOCK_VALUES;
            last_index = end;

            min_ts = ts_sli[start];
            max_ts = ts_sli[end - 1];
            offset = writer.pos();

            // TODO Make encoding result streamable
            let (ts_buf, data_buf) = block.encode(start, end)?;
            // Write u32 hash for timestamps
            writer.write(&crc32fast::hash(&ts_buf).to_be_bytes()[..])
                  .map(|s| blk_size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;
            // Write timestamp blocks
            writer.write(&ts_buf)
                  .map(|s| blk_size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;

            val_off = writer.pos();

            // WRite u32 hash for value blocks
            writer.write(&crc32fast::hash(&data_buf).to_be_bytes()[..])
                  .map(|s| blk_size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;
            // Write value blocks
            writer.write(&data_buf)
                  .map(|s| blk_size += s)
                  .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;

            total_size += blk_size;

            index_buf.insert_block_meta(min_ts, max_ts, offset, blk_size as u64, val_off);

            i += 1;
        }
        index_buf.insert_index_meta(field_id, block_type, block_count);

        Ok(total_size)
    }
}

impl TsmWriter for TsmCacheWriter {
    fn write_header(&mut self) -> Result<usize> {
        write_header_to(&mut self.writer)
    }

    fn write_blocks(&mut self) -> Result<usize> {
        let mut size = 0_usize;
        for (fid, blk) in self.cached_blocks.iter() {
            Self::write_one_block(&mut self.writer, &mut self.index_buf, *fid, blk).map(|i| {
                                                                                       size += i
                                                                                   })?;
        }
        Ok(size)
    }

    fn write_index(&mut self) -> Result<usize> {
        self.index_buf.set_index_offset(self.writer.pos());
        self.index_buf.write_to(&mut self.writer)
    }

    fn write_footer(&mut self) -> Result<usize> {
        write_footer_to(&mut self.writer, &self.index_buf.bloom_filter, self.index_buf.index_offset)
    }

    fn flush(&mut self) -> Result<()> {
        self.writer
            .sync_all(FileSync::Hard)
            .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })
    }
}

pub struct DefaultTsmWriter {
    writer: FileCursor,
    index_buf: IndexBuf,
    size: usize,
}

impl DefaultTsmWriter {
    pub fn new(writer: FileCursor) -> Self {
        Self { writer, index_buf: IndexBuf::new(), size: 0 }
    }
}

impl TsmWriter for DefaultTsmWriter {
    fn write_header(&mut self) -> Result<usize> {
        write_header_to(&mut self.writer)
    }

    fn write_blocks(&mut self) -> Result<usize> {
        todo!()
    }

    fn write_index(&mut self) -> Result<usize> {
        todo!()
    }

    fn write_footer(&mut self) -> Result<usize> {
        write_footer_to(&mut self.writer, &self.index_buf.bloom_filter, self.index_buf.index_offset)
    }

    fn flush(&mut self) -> Result<()> {
        self.writer
            .sync_all(FileSync::Hard)
            .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })
    }
}

pub fn write_header_to(writer: &mut FileCursor) -> Result<usize> {
    let mut size = 0_usize;
    writer.write(&TSM_MAGIC.to_be_bytes().as_ref())
          .and_then(|i| {
              size += i;
              writer.write(&VERSION.to_be_bytes()[..])
          })
          .map(|i| {
              size += i;
          })
          .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;

    Ok(size)
}

pub fn write_footer_to(writer: &mut FileCursor,
                       bloom_filter: &BloomFilter,
                       index_offset: u64)
                       -> Result<usize> {
    let mut size = 0_usize;
    writer.write(&bloom_filter.bytes())
          .and_then(|i| {
              size += i;
              writer.write(&index_offset.to_be_bytes()[..])
          })
          .map(|i| size += i)
          .map_err(|e| Error::WriteTsmErr { reason: e.to_string() })?;

    Ok(size)
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, sync::Arc};

    use logger::info;
    use models::{FieldId, ValueType};

    use super::DefaultTsmWriter;
    use crate::{
        direct_io::FileSync,
        file_manager::{self, get_file_manager, FileManager},
        memcache::{BoolCell, DataType, F64Cell, I64Cell, StrCell, U64Cell},
        tsm::{coders, ColumnReader, DataBlock, IndexReader, TsmCacheWriter, TsmWriter},
    };

    #[test]
    fn test_str_encode() {
        // let block = DataBlock::new(10, crate::DataType::Str(StrCell{ts:1, val: vec![]}));
        // block.insert(crate::DataType::Str(StrCell{ts:1, val: vec![1]}));
        // block.insert(crate::DataType::Str(StrCell{ts:2, val: vec![2]}));
        // block.insert(crate::DataType::Str(StrCell{ts:3, val: vec![3]}));
        let mut data = vec![];
        let str = vec![vec![1_u8]];
        let tmp: Vec<&[u8]> = str.iter().map(|x| &x[..]).collect();
        let _ = coders::string::encode(&tmp, &mut data);
    }

    #[test]
    fn test_tsm_write_fast() {
        let file =
            get_file_manager().create_file("/tmp/test/tsm_writer/tsm_write_fast.tsm").unwrap();
        let data: HashMap<FieldId, DataBlock> =
            HashMap::from([(1, DataBlock::U64 { ts: vec![2, 3, 4], val: vec![12, 13, 15] }),
                           (2, DataBlock::U64 { ts: vec![2, 3, 4], val: vec![101, 102, 103] })]);

        let mut writer = TsmCacheWriter::new(file.into_cursor(), data);
        writer.write().unwrap();

        println!("column write finsh");
        test_tsm_read_fast();
    }

    fn test_tsm_read_fast() {
        let fs = get_file_manager().open_file("/tmp/test/tsm_writer/tsm_write_fast.tsm").unwrap();
        let fs = Arc::new(fs);
        let len = fs.len();

        let index = IndexReader::open(fs.clone()).unwrap();
        let mut column_readers: HashMap<FieldId, ColumnReader> = HashMap::new();
        for index_meta in index.iter() {
            column_readers.insert(index_meta.field_id(),
                                  ColumnReader::new(fs.clone(), index_meta.iter()));
        }

        let ori_data: HashMap<FieldId, Vec<DataBlock>> =
            HashMap::from([(1, vec![DataBlock::U64 { ts: vec![2, 3, 4], val: vec![12, 13, 15] }]),
                           (2,
                            vec![DataBlock::U64 { ts: vec![2, 3, 4], val: vec![101, 102, 103] }])]);

        for (fid, col_reader) in column_readers.iter_mut() {
            dbg!(fid);
            let mut data = Vec::new();
            for block in col_reader.next().unwrap() {
                data.push(block);
            }
            dbg!(&data);

            assert_eq!(*ori_data.get(fid).unwrap(), data);
        }
        info!("read test finish");
    }

    #[test]
    fn test_tsm_write_slow() {
        let mut cache_data: HashMap<FieldId, DataBlock> = HashMap::new();
        // Produce many field_ids, from 1 to 1000
        for i in 1..1000 {
            let fid = i as u64;
            // Use i%5 as ValueType
            let vtyp = ValueType::from((i % 5) as u8);
            cache_data.insert(fid, DataBlock::new(10000, vtyp));
            let blk_ref = cache_data.get_mut(&fid).unwrap();
            // Produce many ts-val pair, ts is from 1 to 10000, val is randomly generated
            for j in 1..10000 {
                let val = match vtyp {
                    ValueType::Unknown => panic!("value type is unknown"),
                    ValueType::Float => {
                        DataType::F64(F64Cell { ts: j, val: rand::random::<f64>() })
                    },
                    ValueType::Integer => {
                        DataType::I64(I64Cell { ts: j, val: rand::random::<i64>() })
                    },
                    ValueType::Unsigned => {
                        DataType::U64(U64Cell { ts: j, val: rand::random::<u64>() })
                    },
                    ValueType::Boolean => {
                        DataType::Bool(BoolCell { ts: j, val: rand::random::<bool>() })
                    },
                    ValueType::String => {
                        DataType::Str(StrCell { ts: j, val: b"hello world".to_vec() })
                    },
                };
                blk_ref.insert(&val);
            }
        }

        // Write to tsm
        let fs = get_file_manager().open_file("/tmp/test/tsm_writer/tsm_write_slow.tsm").unwrap();
        let writer = DefaultTsmWriter::new(fs.into_cursor());
    }
}
