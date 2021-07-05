use crate::read::BoundedReader;
use crate::{BookError, Result};
use aversion::group::{DataSink, DataSourceExt};
use aversion::util::cbor::CborData;
use aversion::{assign_message_ids, UpgradeLatest, Versioned};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use serde::{Deserialize, Serialize};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::thread::panicking;

/// The version of BookWriter being used
const BOOK_V1_MAGIC: u32 = 0xFF33_0001;

/// The fixed size of a header block
const HEADER_SIZE: usize = 4096;

/// The maximum TOC size we will attempt to read
const MAX_TOC_SIZE: u64 = 0x400_0000; // 64MB

/// The `Book` file header struct.
///
/// This is used to communicate that this file is in `Book`
/// format, and what type of data it contains.
#[derive(Debug, Versioned, UpgradeLatest, Serialize, Deserialize)]
pub struct FileHeaderV1 {
    bookwriter_magic: u32,
    pub user_magic: u32,
}

/// A type alias; this will always point to the latest version `FileHeader`.
pub type FileHeader = FileHeaderV1;

/// A `FileSpan` stores the byte offset and length of some range of a file.
///
/// The `FileSpan` deliberately cannot store a zero-length span, because it
/// can be confusing if code attempts to read a zero-sized span. Use
/// `Option<FileSpan` to represent a zero-sized span.
///
#[derive(Debug, Serialize, Deserialize)]
pub struct FileSpanV1 {
    pub offset: u64,
    pub length: NonZeroU64,
}

impl FileSpanV1 {
    /// Create a `FileSpan` from offset and length.
    ///
    /// If `length` is 0, `None` will be returned.
    pub fn from_offset_length(offset: usize, length: usize) -> Option<Self> {
        let offset = offset as u64;
        let length = length as u64;
        // Try to create a NonZeroU64 length; if that returns Some(l)
        // then return Some(FileSpan{..}) else None.
        NonZeroU64::new(length).map(|length| FileSpanV1 { offset, length })
    }
}

// A type alias, to make code a little easier to read.
type FileSpan = FileSpanV1;

/// A Table-of-contents entry.
///
/// This contains an identifying number, and a file span that
/// tells us what chunk of the file contains this chapter.
#[derive(Debug, Serialize, Deserialize)]
pub struct TocEntryV1 {
    pub id: u64,
    pub span: Option<FileSpanV1>,
}

// A type alias, to make code a little easier to read.
type TocEntry = TocEntryV1;

/// A Table-of-contents.
///
/// This contains multiple `TocEntry` values, one for each chapter.
#[derive(Debug, Default, Serialize, Deserialize, Versioned, UpgradeLatest)]
pub struct TocV1(Vec<TocEntryV1>);

// A type alias, used by the Versioned trait.
type Toc = TocV1;

impl Toc {
    fn add(&mut self, entry: TocEntry) {
        self.0.push(entry);
    }

    fn iter(&self) -> impl Iterator<Item = &TocEntry> {
        self.0.iter()
    }

    fn get_chapter(&self, index: ChapterIndex) -> Result<&TocEntry> {
        self.0.get(index.0).ok_or(BookError::NoChapter)
    }
}

assign_message_ids! {
    FileHeader: 1,
    Toc: 2,
}

/// A tool for writing a `Chapter`.
///
/// A `ChapterWriter` creates a new chapter. Chapters will be written
/// sequentially in the file, and only one can be active at a time.
///
/// To write the chapter data, use the [`Write`] interface.
///
/// When the chapter is complete, call [`close()`] to flush any
/// remaining bytes and update the `Book` table-of-contents.
///
/// Attempting to drop a chapter without calling `close` will
/// cause a panic.
///
/// See [`BookWriter`] for more information.
///
/// [`close()`]: Self::close
pub struct ChapterWriter<'a, W> {
    writer: &'a mut W,
    toc: &'a mut Toc,
    id: u64,
    offset: usize,
    length: usize,
}

impl<'a, W> ChapterWriter<'a, W>
where
    W: Write,
{
    /// Create a new `ChapterWriter`.
    fn new(book: &'a mut BookWriter<W>, id: u64, offset: usize) -> Self {
        ChapterWriter {
            writer: &mut book.writer,
            toc: &mut book.toc,
            id,
            offset,
            length: 0,
        }
    }

    /// Complete the chapter.
    ///
    /// `Chapter` instances should not be dropped; they must be consumed
    /// by calling `close`. This allows us to detect any final IO errors
    /// and update the TOC.
    pub fn close(mut self) -> Result<()> {
        self.flush()?;

        let toc_entry = TocEntry {
            id: self.id,
            span: FileSpan::from_offset_length(self.offset, self.length),
        };

        self.toc.add(toc_entry);

        // Mark this Chapter as safe to drop.
        self.length = 0;

        Ok(())
    }
}

impl<W> Drop for ChapterWriter<'_, W> {
    fn drop(&mut self) {
        // A `Chapter` must not be dropped if it has contents,
        // because we want the owner to call [`close`] and handle
        // any IO errors.
        if self.length != 0 {
            // We don't want to panic if the Chapter is being dropped
            // while unwinding.
            if !panicking() {
                panic!("Chapter was dropped without calling close()");
            }
        }
    }
}

impl<'a, W> Write for ChapterWriter<'a, W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes_written = self.writer.write(buf)?;
        self.length += bytes_written;
        Ok(bytes_written)
    }

    // Note `close` will call `flush` automatically.
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A tool for writing a `Book`.
///
/// A `BookWriter` creates a new `Book`.
///
/// To write a chapter, call [`new_chapter()`].
///
/// When the book is complete, call [`close()`] to flush any
/// remaining bytes and write out the table of contents.
///
/// [`close()`]: Self::close
/// [`new_chapter()`]: Self::new_chapter
///
#[derive(Debug)]
pub struct BookWriter<W: Write> {
    writer: W,
    current_offset: usize,
    header: FileHeader,
    toc: Toc,
}

impl<W: Write> BookWriter<W> {
    /// Create a new `BookWriter`.
    ///
    /// `user_magic` is a number stored in the file for later identification.
    /// It can contain any value the user wants, and can be used to
    /// disambiguate different kinds of files.
    ///
    pub fn new(writer: W, user_magic: u32) -> Result<Self> {
        let mut this = BookWriter {
            writer,
            current_offset: 0,
            header: FileHeader {
                bookwriter_magic: BOOK_V1_MAGIC,
                user_magic,
            },
            toc: Toc::default(),
        };
        this.write_header()?;
        Ok(this)
    }

    fn write_header(&mut self) -> Result<()> {
        // Serialize the header into a buffer.
        let header_buf = Cursor::new(Vec::<u8>::new());
        let mut header_writer = CborData::new(header_buf);
        header_writer.write_message(&self.header)?;

        let mut header_buf = header_writer.into_inner().into_inner();
        if header_buf.len() > HEADER_SIZE {
            panic!("serialized header exceeds maximum size");
        }
        // Pad the buffer with zeroes so that it's the expected
        // size.
        header_buf.resize(HEADER_SIZE, 0);

        // FIXME: wrap the writer in some struct that automatically counts
        // the number of bytes written.
        self.writer.write_all(&header_buf)?;
        self.current_offset = HEADER_SIZE;
        Ok(())
    }

    /// Create a new `ChapterWriter`.
    ///
    /// The chapter `id` can be any value the user wants, and can be
    /// used to later locate a chapter.
    ///
    pub fn new_chapter(&mut self, id: u64) -> ChapterWriter<'_, W> {
        ChapterWriter::new(self, id, self.current_offset)
    }

    /// Finish writing the `Book` file.
    ///
    /// On success, this returns the original writer stream.
    /// It is normal to discard it, except in unit tests.
    pub fn close(mut self) -> Result<W> {
        // Serialize the TOC into a buffer.
        let toc_buf = Cursor::new(Vec::<u8>::new());
        let mut toc_writer = CborData::new(toc_buf);
        toc_writer.write_message(&self.toc)?;
        let mut toc_buf = toc_writer.into_inner().into_inner();

        // Manually serialize the TOC length, so that it has a fixed size and
        // a fixed offset (relative to the end of the file).
        let toc_length = toc_buf.len() as u64;
        toc_buf.write_u64::<BigEndian>(toc_length).unwrap();

        // Write the TOC.
        self.writer.write_all(&toc_buf)?;

        // TODO: Add a checksum.

        self.writer.flush()?;
        Ok(self.writer)
    }
}

/// A chapter index.
pub struct ChapterIndex(pub usize);

/// An interface for reading a Bookfile.
///
#[derive(Debug)]
pub struct Book<R> {
    reader: R,
    header: FileHeader,
    toc: Toc,
}

impl<R> Book<R>
where
    R: Read + Seek,
{
    /// Create a new Book from a stream
    ///
    /// This call will attempt to read the file header and table of contents.
    /// It may fail due to IO errors while reading, or invalid file data.
    ///
    /// The stream must impl the `Read` and `Seek` traits (e.g. a `File`).
    ///
    pub fn new(mut reader: R) -> Result<Self> {
        // Read the header from the beginning of the file.
        let mut header_buf = [0u8; HEADER_SIZE];
        reader.seek(SeekFrom::Start(0))?;
        reader.read_exact(&mut header_buf)?;
        let buf_reader = &header_buf[..];

        let mut data_src = CborData::new(buf_reader);
        let header: FileHeader = data_src.expect_message()?;

        // Verify magic numbers
        if header.bookwriter_magic != BOOK_V1_MAGIC {
            return Err(BookError::Serializer);
        }

        // Read the TOC length. For v1 it is the last 8 bytes of the file.
        let toc_end = reader.seek(SeekFrom::End(-8))?;
        let toc_len = reader.read_u64::<BigEndian>()?;
        if toc_len > MAX_TOC_SIZE {
            return Err(BookError::Serializer);
        }

        // Deserialize the TOC.
        let toc_offset = toc_end - toc_len;
        let toc_reader = BoundedReader::new(&mut reader, toc_offset, toc_len);
        let mut data_src = CborData::new(toc_reader);
        let toc: Toc = data_src.expect_message().unwrap();

        Ok(Book {
            reader,
            header,
            toc,
        })
    }

    /// Look up a chapter.
    ///
    /// For now, we assume chapter ids are unique. That's dumb,
    /// and will be fixed in a future version.
    pub fn find_chapter(&self, id: u64) -> Option<ChapterIndex> {
        for (index, entry) in self.toc.iter().enumerate() {
            if entry.id == id {
                return Some(ChapterIndex(index));
            }
        }
        None
    }

    /// Read a chapter by index.
    pub fn chapter_reader(&mut self, index: ChapterIndex) -> Result<BoundedReader<R>> {
        let toc_entry = self.toc.get_chapter(index)?;
        match &toc_entry.span {
            None => {
                // If the span is empty, no IO is necessary; just return
                // an empty Vec.
                Ok(BoundedReader::empty(&mut self.reader))
            }
            Some(span) => {
                self.reader.seek(SeekFrom::Start(span.offset))?;
                Ok(BoundedReader::new(
                    &mut self.reader,
                    span.offset,
                    span.length.into(),
                ))
            }
        }
    }

    /// Read all bytes in a chapter.
    ///
    /// This is the same thing as calling [`chapter_reader`] followed by
    /// `read_to_end`.
    pub fn read_chapter(&mut self, index: ChapterIndex) -> Result<Box<[u8]>> {
        let mut buf = vec![];
        let mut reader = self.chapter_reader(index)?;
        reader.read_to_end(&mut buf)?;
        Ok(buf.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::io::Cursor;

    #[test]
    fn empty_book() {
        let magic = 0x1234;
        let mut cursor = Cursor::new(Vec::<u8>::new());
        {
            let book = BookWriter::new(&mut cursor, magic).unwrap();
            book.close().unwrap();
        }

        // This file contains only a header, an empty TOC, and a TOC-length.
        assert_eq!(cursor.get_ref().len(), 4096 + 9 + 8);

        // If this succeeds then the header and TOC were parsed correctly.
        let _ = Book::new(cursor).unwrap();
    }

    #[test]
    fn simple_book() {
        let magic = 0x1234;
        let buffer = {
            let buffer = Cursor::new(Vec::<u8>::new());
            let mut book = BookWriter::new(buffer, magic).unwrap();
            let chapter = book.new_chapter(11);
            chapter.close().unwrap();
            let mut chapter = book.new_chapter(22);
            chapter.write_all(b"This is chapter 22").unwrap();
            chapter.close().unwrap();
            book.close().unwrap()
        };
        let mut book = Book::new(buffer).unwrap();
        let n = book.find_chapter(11).unwrap();
        let ch1 = book.read_chapter(n).unwrap();
        assert!(ch1.is_empty());

        assert!(book.find_chapter(1).is_none());

        let n = book.find_chapter(22).unwrap();
        let ch2 = book.read_chapter(n).unwrap();
        assert_eq!(ch2.as_ref(), b"This is chapter 22");
    }
}
