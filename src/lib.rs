extern crate unarr_sys;

#[cfg(not(feature = "no_guess"))]
extern crate chardet;
#[cfg(not(feature = "no_guess"))]
extern crate encoding;
#[cfg(not(feature = "no_guess"))]
extern crate codepage_437;

use unarr_sys::ffi::*;

#[cfg(not(feature = "no_guess"))]
use encoding::label::encoding_from_whatwg_label;
#[cfg(not(feature = "no_guess"))]
use encoding::DecoderTrap;



#[cfg(not(feature = "no_guess"))]
use codepage_437::{ToCp437, CP437_WINGDINGS};

use std::{
    ffi::{CStr, CString},
    path::Path,
};

const SKIP_BUF_SIZE: usize = 1024 * 1024 * 1024;
type Cookie = u64;
const INVALID_READER_COOKIE: Cookie = 0;

pub struct ArStream {
    ptr: p_ar_stream,
    mem: Option<Vec<u8>>,
}

pub struct EntryReader<'a> {
    archive: &'a ArArchive,
    entry_offset: off64_t,
    readed: size_t,
    size: size_t,
    skip_buf: *mut u8,
    cookie: Cookie,
}

unsafe fn skip_buf_layout() -> std::alloc::Layout {
    std::alloc::Layout::from_size_align_unchecked(SKIP_BUF_SIZE, 1)
}

impl<'a> Drop for EntryReader<'a> {
    fn drop(&mut self) {
        if !self.skip_buf.is_null() {
            unsafe {
                std::alloc::dealloc(self.skip_buf, skip_buf_layout());
            }
        }
    }
}

impl<'a> EntryReader<'a> {
    unsafe fn resume(&mut self) -> std::io::Result<()> {
        let need_reset_pos = (ar_entry_get_offset(self.archive.ptr) != self.entry_offset)
            || (self.archive.last_reader_cookie.get() != self.cookie);

        if !need_reset_pos {
            return Ok(());
        }
        self.archive.last_reader_cookie.set(self.cookie);
        if !ar_parse_entry_at(self.archive.ptr, self.entry_offset) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "reset archive offset failed",
            ));
        }
        //must resume last read pos. read up to readed bytes
        //allocate temp memory to write unused bytes
        if self.skip_buf.is_null() && self.readed > 0 {
            //lazy create a 1MB buffer to skip bytes
            //maybe we can use stack buf to avoid this, but stack
            //maybe too small for quickly unpack enough bytes
            self.skip_buf = std::alloc::alloc(skip_buf_layout());
        }

        let mut skip = self.readed;
        while skip > 0 {
            let to_read = skip.min(SKIP_BUF_SIZE);
            if !ar_entry_uncompress(self.archive.ptr, self.skip_buf as *mut c_void, to_read) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "skip buffer failed",
                ));
            }
            skip -= to_read;
        }

        Ok(())
    }
}

impl<'a> std::io::Read for EntryReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        assert!(self.readed <= self.size);

        if self.readed == self.size {
            //already EOF
            return Ok(0);
        }

        //we must check if caller changed to use other entry
        //or created another reader
        unsafe {
            self.resume()?;
        }

        let to_read = (self.size - self.readed).min(buf.len());

        unsafe {
            if ar_entry_uncompress(self.archive.ptr, buf.as_mut_ptr() as *mut c_void, to_read) {
                self.readed += to_read;
                assert!(self.readed <= self.size);
                return Ok(to_read);
            }
        }

        //we always read-equal to left bytes, if still failed
        //it must be IO error

        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "failed to read",
        ))
    }
}

pub struct ArArchive {
    ptr: p_ar_archive,
    // unarr didn't force 1:1 relationship between stream:archive
    // but ar_stream in unarr is almost for abstract file and mem
    // they're useless out of Archive usage, so we bind their lifetime together
    // makes ArArchive self-contained ,without extra lifetime headache
    stream: std::mem::ManuallyDrop<ArStream>,
    cookie_counter: std::cell::Cell<Cookie>,
    last_reader_cookie: std::cell::Cell<Cookie>,
    #[cfg(feature = "chardet")]
    format: ArchiveFormat,
}

unsafe impl Send for ArArchive {}

impl Drop for ArArchive {
    fn drop(&mut self) {
        unsafe {
            ar_close_archive(self.ptr);
            //we need to destory ar_archive first ,
            //and then the underline stream
            std::mem::ManuallyDrop::drop(&mut self.stream);
        }
    }
}

impl Drop for ArStream {
    fn drop(&mut self) {
        unsafe {
            ar_close(self.ptr);
        }
    }
}

impl ArStream {
    pub fn from_file<P: AsRef<Path>>(path: P) -> std::io::Result<ArStream> {
        let path_str_c = CString::new(path.as_ref().as_os_str().to_str().unwrap()).unwrap();

        let ptr: p_ar_stream;

        unsafe {
            ptr = ar_open_file(path_str_c.as_ptr());
        }

        if ptr.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "create ar_stream from file failed",
            ));
        }

        Ok(ArStream { ptr, mem: None })
    }

    pub fn from_memory(memory: Vec<u8>) -> ArStream {
        let mut ret = ArStream {
            ptr: std::ptr::null(),
            mem: Some(memory),
        };

        let p: p_ar_stream;
        unsafe {
            p = ar_open_memory(
                ret.mem.as_ref().unwrap().as_ptr() as *const c_void,
                ret.mem.as_ref().unwrap().len(),
            );
        }

        ret.ptr = p;

        ret
    }
}

#[derive(Copy, Clone)]
pub enum ArchiveFormat {
    Zip,
    Rar,
    _7z,
    Tar,
}

impl ArArchive {
    pub fn iter(&self) -> ArArchiveIterator {
        ArArchiveIterator {
            archive: self,
            entry_offset: 0,
        }
    }

    pub fn reader_for<'a>(&'a self, entry: &ArEntry) -> std::io::Result<EntryReader<'a>> {
        //entry must be read from this archive
        #[cfg(debug_assertions)]
        assert!(entry.ptr == self.ptr);

        let ok: bool;

        unsafe {
            ok = ar_parse_entry_at(self.ptr, entry.offset);
        }

        if !ok {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "can not parse for entry",
            ));
        }

        self.cookie_counter.set(self.cookie_counter.get() + 1);

        let ret = EntryReader {
            archive: self,
            readed: 0,
            size: entry.size,
            entry_offset: entry.offset,
            skip_buf: std::ptr::null_mut(),
            cookie: self.cookie_counter.get(),
        };

        Ok(ret)
    }

    pub fn new(stream: ArStream, try_format: Option<ArchiveFormat>) -> std::io::Result<ArArchive> {
        let mut ptr: p_ar_archive;

        let mut tries = vec![];
        if let Some(v) = try_format {
            tries.push(v);
        } else {
            tries.push(ArchiveFormat::Zip);
            tries.push(ArchiveFormat::Rar);
            tries.push(ArchiveFormat::_7z);
            tries.push(ArchiveFormat::Tar);
        }

        for try_format in tries.iter() {
            unsafe {
                match try_format {
                    ArchiveFormat::Zip => {
                        ptr = ar_open_zip_archive(stream.ptr, false);
                    }
                    ArchiveFormat::Rar => {
                        ptr = ar_open_rar_archive(stream.ptr);
                    }
                    ArchiveFormat::_7z => {
                        ptr = ar_open_7z_archive(stream.ptr);
                    }
                    ArchiveFormat::Tar => {
                        ptr = ar_open_tar_archive(stream.ptr);
                    }
                }
            }

            if !ptr.is_null() {
                return Ok(ArArchive {
                    ptr,
                    stream: std::mem::ManuallyDrop::new(stream),
                    cookie_counter: std::cell::Cell::new(INVALID_READER_COOKIE),
                    last_reader_cookie: std::cell::Cell::new(INVALID_READER_COOKIE),
                    #[cfg(feature = "chardet")]
                    format: *try_format,
                });
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "create archive failed",
        ))
    }
}

pub struct ArArchiveIterator<'a> {
    archive: &'a ArArchive,
    entry_offset: off64_t,
}

// ArEntry just keep attr as fields. unarr holds current
pub struct ArEntry {
    //at next ar_parse_entry call ,previous name is invalid , so we must make
    //this field owned (copy from c)
    name: String,
    offset: off64_t,
    size: size_t,
    time: time64_t,

    #[cfg(debug_assertions)]
    ptr: p_ar_archive,
}

impl ArEntry {
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn size(&self) -> size_t {
        self.size
    }

    pub fn time(&self) -> time64_t {
        self.time
    }
}

#[cfg(not(feature = "no_guess"))]
fn zip_guess_name(cstr: &CStr) -> Option<String> {

    //try convert back to raw string. original input can be 
    //just UTF8 or incorrected converted to utf8 from cp437
    //
    let try_as_utf8 = cstr.to_str();

    if try_as_utf8.is_err() {
        return None;
    }

    let try_back_437= try_as_utf8.unwrap().to_cp437(&CP437_WINGDINGS);

    if try_back_437.is_err() {
        // can not convert back to cp437
        // since cp437 has 1:1 utf8 mapping
        // so this utf8 can't be converted from cp437

        return Some(try_as_utf8.unwrap().to_owned());
        
    }

    //successfully convert back to cp437 , two possible:
    //1. raw string from zip is just ascii, convert to utf8 without change value
    //2. raw string is not cp437, and has other character which is not normal
    //    file name char. 
    //no matter which condition, we just guess encoding by chardet

    let cp437bin = try_back_437.unwrap();

    //guess encoding
    let result = chardet::detect(&cp437bin);
    // result.0 Encode
    // result.1 Confidence
    // result.2 Language

    // decode file into utf-8
    let dec = encoding_from_whatwg_label(chardet::charset2encoding(&result.0))?;

    let decoded = dec.decode(&cp437bin, DecoderTrap::Ignore);
    if decoded.is_err() {
        return None;
    }

    Some(decoded.unwrap())
}

impl<'a> Iterator for ArArchiveIterator<'a> {
    type Item = ArEntry;

    fn next(&mut self) -> Option<ArEntry> {
        let parse_ok: bool;

        self.archive.last_reader_cookie.set(INVALID_READER_COOKIE);

        unsafe {
            if self.entry_offset == 0 {
                parse_ok = ar_parse_entry_at(self.archive.ptr, 0);
            } else {
                //must check if other call (reader) changed current offset
                let current_offset = ar_entry_get_offset(self.archive.ptr);
                if current_offset != self.entry_offset {
                    let result = ar_parse_entry_at(self.archive.ptr, self.entry_offset);
                    if !result {
                        return None;
                    }
                }
                parse_ok = ar_parse_entry(self.archive.ptr);
            }
        }

        //can not parse , maybe already EOF
        //or advise to next reached EOF
        if !parse_ok {
            //if parse entry failed, archive may not advise anymore
            //so return
            return None;
        }

        //now we already parsed a entry

        let name: String;

        let offset: off64_t;
        let size: size_t;
        let time: time64_t;
        unsafe {
            let c_name = ar_entry_get_name(self.archive.ptr);
            assert!(!c_name.is_null());

            #[cfg(not(feature = "no_guess"))]
            {
                let c_str = CStr::from_ptr(c_name);
                if let ArchiveFormat::Zip = self.archive.format {
                    let guessed = zip_guess_name(c_str);
                    if guessed.is_none() {
                        //unarr try to decode as CP437 if not a utf8
                        //encoding so we can assume the string is utf8
                        //encoded (all value has corresponding utf8 represent)
                        name = c_str.to_str().unwrap().to_string();
                    } else {
                        name = guessed.unwrap();
                    }
                } else {
                    name = c_str.to_str().unwrap().to_string();
                }
            }

            #[cfg(feature = "no_guess")]
            {
                name = CStr::from_ptr(c_name).to_string_lossy();
            }

            offset = ar_entry_get_offset(self.archive.ptr);
            size = ar_entry_get_size(self.archive.ptr);
            time = ar_entry_get_filetime(self.archive.ptr);
        }

        let ret = ArEntry {
            name,
            offset,
            size,
            time,
            #[cfg(debug_assertions)]
            ptr: self.archive.ptr,
        };

        assert!(ret.offset == 0 || (ret.offset > self.entry_offset));
        self.entry_offset = ret.offset;

        Some(ret)
    }
}

#[cfg(test)]
extern crate rand;

mod tests {

    use super::*;
    use std::io::Read;

    #[test]
    fn test_encoding() {
        let ar = ArArchive::new(
            ArStream::from_file("/home/comicfans/Downloads/中债国债到期收益率.zip")
                .unwrap(),
            None,
        )
        .unwrap();

        for ent in ar.iter() {
            println!("{}", ent.name());
        }
    }

    #[test]
    fn test() {
        let mut d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        d.push("tests/test.zip");

        let from_file = ArStream::from_file(d).unwrap();

        let ar = ArArchive::new(from_file, None).unwrap();

        let entries: Vec<ArEntry> = ar.iter().collect();

        for (i, f) in ar.iter().enumerate() {
            let mut outer_buf = vec![0u8; f.size];
            let mut outer_reader = ar.reader_for(&f).unwrap();

            let read_first = rand::random::<usize>() % f.size;
            let _ = outer_reader.read_exact(&mut outer_buf[..read_first]);

            let mut inner_vec = Vec::new();
            for change_pos in entries.iter() {
                let mut bin: Vec<u8> = Vec::new();
                let mut reader = ar.reader_for(&change_pos).unwrap();
                let _ = reader.read_to_end(&mut bin);
                inner_vec.push(bin);
            }

            let _ = outer_reader.read_exact(&mut outer_buf[read_first..]);
            assert_eq!(outer_buf, inner_vec[i]);
        }
    }
}
