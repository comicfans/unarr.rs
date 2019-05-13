extern crate unarr_sys;

use unarr_sys::ffi::*;

use std::{
    ffi::{CStr, CString},
    path::Path,
};

pub struct ArStream {
    ptr: p_ar_stream,
    mem: Option<Vec<u8>>
}

pub struct EntryReader{
    ptr:p_ar_archive,
    entry_offset: off64_t,
    readed: size_t,
    size: size_t,
    skip_buf: *mut u8
}

const SKIP_BUF_SIZE : usize = 1024*1024*1024;
fn skip_buf_layout()->std::alloc::Layout{
    std::alloc::Layout::from_size_align_unchecked(SKIP_BUF_SIZE,1)
}

impl Drop for EntryReader {
    fn drop(&mut self){
        if !self.skip_buf.is_null(){
            std::alloc::dealloc(self.skip_buf, skip_buf_layout());
        }
    }
}

impl std::io::Read  for EntryReader{

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>{

        assert!(self.readed <= self.size);

        if self.readed == self.size {
            //already EOF
            return Ok(0);
        }

        //we must check if caller changed to use other entry
        unsafe{
            if ar_entry_get_offset(self.ptr) != self.entry_offset {
                if !ar_parse_entry_at(self.ptr, self.entry_offset) {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,"reset archive offset failed"));
                }
                //must resume last read pos. read up to readed bytes
                //allocate temp memory to write unused bytes
                if self.skip_buf.is_null() {
                    //lazy create a 1MB buffer to skip bytes
                    //maybe we can use stack buf to avoid this, but stack
                    //maybe too small for quickly unpack enough bytes
                    self.skip_buf = std::alloc::alloc(skip_buf_layout());
                }

                let skip = self.readed;
                while skip > 0 {
                    let to_read = skip.min(SKIP_BUF_SIZE);
                    if ! ar_entry_uncompress(self.ptr,self.skip_buf as *mut c_void, to_read){
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,"skip buffer failed"));
                    }
                    skip -= to_read;
                }
            }
        }

        let to_read = (self.size - self.readed).min(buf.len());

        
        unsafe{
            if ar_entry_uncompress(self.ptr, buf.as_mut_ptr() as *mut c_void, to_read){
                return Ok(to_read);
            }
        }

        //we always read-equal to left bytes, if still failed 
        //it must be IO error

        return Err(std::io::Error::new(std::io::ErrorKind::Other,"failed to read"));
    }
}

pub struct ArArchive {
    ptr: p_ar_archive,

    // unarr didn't force 1:1 relationship between stream:archive
    // but ar_stream in unarr is almost for abstract file and mem
    // they're useless out of Archive usage, so we bind their lifetime together
    // makes ArArchive self-contained ,without extra lifetime headache
    stream: std::mem::ManuallyDrop<ArStream>,
    reader: EntryReader
}

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
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,"create ar_stream from file failed"));


        }

        return Ok(ArStream { ptr: ptr,mem: None });
    }

    pub fn from_memory(memory:Vec<u8>)->ArStream {

        let mut ret = ArStream{
            ptr:std::ptr::null(),
            mem:Some(memory)
        };

        let p: p_ar_stream;
        unsafe{
            p = ar_open_memory(ret.mem.as_ref().unwrap().as_ptr() as *const c_void, ret.mem.as_ref().unwrap().len());
        }

        ret.ptr = p;

        return ret;
        
    }
}

pub enum TryFormat {
    Zip,
    Rar,
    _7z,
    Tar,
}

impl ArArchive {

    pub fn iter(&mut self)->ArArchiveIterator {
        ArArchiveIterator{
            archive:self,
            entry_offset:0
        }
    }

    pub fn reader_for(&mut self, entry: &ArEntry) -> std::io::Result<&mut EntryReader> {

        //entry must be read from this archive
        #[cfg(debug)]
        assert!(entry.ptr == self.ptr);

        let ok:bool;
        unsafe{
            ok = ar_parse_entry_at(self.ptr, entry.offset);
        }

        if !ok {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,"can not parse for entry"));
        }

        self.reader.readed = 0;
        self.reader.size = entry.size;
        self.reader.entry_offset = entry.offset;

        return Ok(&mut self.reader);
    }

    pub fn new(stream: ArStream, try_format: Option<TryFormat>) -> std::io::Result<ArArchive> {
        let mut ptr: p_ar_archive;

        let mut tries = vec![];
        if let Some(v) = try_format {
            tries.push(v);
        } else {
            tries.push(TryFormat::Zip);
            tries.push(TryFormat::Rar);
            tries.push(TryFormat::_7z);
            tries.push(TryFormat::Tar);
        }

        for try_format in tries.iter() {
            unsafe {
                match try_format {
                    TryFormat::Zip => {
                        ptr = ar_open_zip_archive(stream.ptr, false);
                    }
                    TryFormat::Rar => {
                        ptr = ar_open_rar_archive(stream.ptr);
                    }
                    TryFormat::_7z => {
                        ptr = ar_open_7z_archive(stream.ptr);
                    }
                    TryFormat::Tar => {
                        ptr = ar_open_tar_archive(stream.ptr);
                    }
                }
            }

            if !ptr.is_null() {
                return Ok(ArArchive {
                    ptr: ptr,
                    stream: std::mem::ManuallyDrop::new(stream),
                    reader: EntryReader{
                        readed:0,
                        size:0,
                        ptr:ptr,
                        entry_offset:0,
                        skip_buf: std::ptr::null_mut()
                    }
                });
            }
        }

        return Err(std::io::Error::new(std::io::ErrorKind::NotFound,"create archive failed"));
    }
}

pub struct ArArchiveIterator<'a> {
    archive: &'a mut ArArchive,
    entry_offset: off64_t
}

// ArEntry just keep attr as fields. unarr holds current 
pub struct ArEntry{
    //at next ar_parse_entry call ,previous name is invalid , so we must make
    //this field owned (copy from c)
    pub name:  String,
    pub offset: off64_t,
    pub size: size_t,
    pub time: time64_t,

    #[cfg(debug)]
    ptr: p_ar_archive
}


impl <'a>Iterator for ArArchiveIterator<'a> {
    type Item = ArEntry;


    fn next(&mut self) -> Option<ArEntry> {
            let parse_ok: bool;

            unsafe {
                if self.entry_offset == 0{

                    parse_ok = ar_parse_entry_at(self.archive.ptr, 0);
                } else {

                    //must check if other call (reader) changed current offset
                    unsafe{
                        let current_offset = ar_entry_get_offset(self.archive.ptr);
                        if current_offset != self.entry_offset {
                            let result = ar_parse_entry_at(self.archive.ptr, self.entry_offset);
                            if !result {
                                return None;
                            }
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
            let filetime: time64_t;
            unsafe {
                let c_name = ar_entry_get_name(self.archive.ptr);
                assert!(!c_name.is_null());

                //unarr file name is UTF8 encoded
                name = CStr::from_ptr(c_name).to_str().unwrap().into();
                offset = ar_entry_get_offset(self.archive.ptr);
                size = ar_entry_get_size(self.archive.ptr);
                filetime = ar_entry_get_filetime(self.archive.ptr);
            }

            let ret = ArEntry{
                name: name,
                offset: offset,
                size: size,
                time: filetime,
                #[cfg(debug)]
                ptr: self.archive.ptr,
            };

            assert!(!ret.offset > self.entry_offset);
            self.entry_offset = ret.offset;

            return Some(ret);
    }
}

#[test]
fn test(){

    let from_file = ArStream::from_file("/home/wangxinyu/Downloads/logtrail-6.6.1-0.1.31.zip").unwrap();
    let mut ar = ArArchive::new(from_file,None).unwrap();

    for f in ar.iter(){
        println!("{}",f.name);
    }
}
