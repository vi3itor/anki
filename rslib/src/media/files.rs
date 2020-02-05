// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use crate::err::{AnkiError, Result};
use lazy_static::lazy_static;
use log::debug;
use regex::Regex;
use sha1::Sha1;
use std::borrow::Cow;
use std::io::Read;
use std::path::Path;
use std::{fs, io, time};
use trash::remove_all;
use unicode_normalization::{is_nfc, UnicodeNormalization};

/// The maximum length we allow a filename to be. When combined
/// with the rest of the path, the full path needs to be under ~240 chars
/// on some platforms, and some filesystems like eCryptFS will increase
/// the length of the filename.
pub(super) static MAX_FILENAME_LENGTH: usize = 120;

/// Media syncing does not support files over 100MiB.
pub(super) static MEDIA_SYNC_FILESIZE_LIMIT: usize = 100 * 1024 * 1024;

lazy_static! {
    static ref WINDOWS_DEVICE_NAME: Regex = Regex::new(
        r#"(?xi)
            # starting with one of the following names
            ^
            (
                CON | PRN | AUX | NUL | COM[1-9] | LPT[1-9]
            )
            # either followed by a dot, or no extension
            (
                \. | $
            )
        "#
    )
    .unwrap();
    pub(super) static ref NONSYNCABLE_FILENAME: Regex = Regex::new(
        r#"(?xi)
            ^
            (:?
                thumbs.db | .ds_store
            )
            $
            "#
    )
    .unwrap();
}

/// True if character may cause problems on one or more platforms.
fn disallowed_char(char: char) -> bool {
    match char {
        '[' | ']' | '<' | '>' | ':' | '"' | '/' | '?' | '*' | '^' | '\\' | '|' => true,
        c if c.is_ascii_control() => true,
        _ => false,
    }
}

/// Adjust filename into the format Anki expects.
///
/// - The filename is normalized to NFC.
/// - Any problem characters are removed.
/// - Windows device names like CON and PRN have '_' appended
/// - The filename is limited to 120 bytes.
pub(crate) fn normalize_filename(fname: &str) -> Cow<str> {
    let mut output = Cow::Borrowed(fname);

    if !is_nfc(output.as_ref()) {
        output = output.chars().nfc().collect::<String>().into();
    }

    if output.chars().any(disallowed_char) {
        output = output.replace(disallowed_char, "").into()
    }

    if let Cow::Owned(o) = WINDOWS_DEVICE_NAME.replace_all(output.as_ref(), "${1}_${2}") {
        output = o.into();
    }

    if let Cow::Owned(o) = truncate_filename(output.as_ref(), MAX_FILENAME_LENGTH) {
        output = o.into();
    }

    output
}

/// Write desired_name into folder, renaming if existing file has different content.
/// Returns the used filename.
pub fn add_data_to_folder_uniquely<'a, P>(
    folder: P,
    desired_name: &'a str,
    data: &[u8],
    sha1: [u8; 20],
) -> io::Result<Cow<'a, str>>
where
    P: AsRef<Path>,
{
    let normalized_name = normalize_filename(desired_name);

    let mut target_path = folder.as_ref().join(normalized_name.as_ref());

    let existing_file_hash = existing_file_sha1(&target_path)?;
    if existing_file_hash.is_none() {
        // no file with that name exists yet
        fs::write(&target_path, data)?;
        return Ok(normalized_name);
    }

    if existing_file_hash.unwrap() == sha1 {
        // existing file has same checksum, nothing to do
        return Ok(normalized_name);
    }

    // give it a unique name based on its hash
    let hashed_name = add_hash_suffix_to_file_stem(normalized_name.as_ref(), &sha1);
    target_path.set_file_name(&hashed_name);

    fs::write(&target_path, data)?;
    Ok(hashed_name.into())
}

/// Convert foo.jpg into foo-abcde12345679.jpg
fn add_hash_suffix_to_file_stem(fname: &str, hash: &[u8; 20]) -> String {
    // when appending a hash to make unique, it will be 20 bytes plus the hyphen.
    let max_len = MAX_FILENAME_LENGTH - 20 - 1;

    let (stem, ext) = split_and_truncate_filename(fname, max_len);

    format!("{}-{}.{}", stem, hex::encode(hash), ext)
}

/// If filename is longer than max_bytes, truncate it.
fn truncate_filename(fname: &str, max_bytes: usize) -> Cow<str> {
    if fname.len() <= max_bytes {
        return Cow::Borrowed(fname);
    }

    let (stem, ext) = split_and_truncate_filename(fname, max_bytes);

    format!("{}.{}", stem, ext).into()
}

/// Split filename into stem and extension, and trim both so the
/// resulting filename would be under max_bytes.
/// Returns (stem, extension)
fn split_and_truncate_filename(fname: &str, max_bytes: usize) -> (&str, &str) {
    // the code assumes the length will be at least 11
    debug_assert!(max_bytes > 10);

    let mut iter = fname.rsplitn(2, '.');
    let mut ext = iter.next().unwrap();
    let mut stem = if let Some(s) = iter.next() {
        s
    } else {
        // no extension, so ext holds the full filename
        let ext_tmp = ext;
        ext = "";
        ext_tmp
    };

    // cap extension to 10 bytes so stem_len can't be negative
    ext = truncate_to_char_boundary(ext, 10);

    // cap stem, allowing for the .
    let stem_len = max_bytes - ext.len() - 1;
    stem = truncate_to_char_boundary(stem, stem_len);

    (stem, ext)
}

/// Trim a string on a valid UTF8 boundary.
/// Based on a funtion in the Rust stdlib.
fn truncate_to_char_boundary(s: &str, mut max: usize) -> &str {
    if max >= s.len() {
        s
    } else {
        while !s.is_char_boundary(max) {
            max -= 1;
        }
        &s[..max]
    }
}

/// Return the SHA1 of a file if it exists, or None.
fn existing_file_sha1(path: &Path) -> io::Result<Option<[u8; 20]>> {
    match sha1_of_file(path) {
        Ok(o) => Ok(Some(o)),
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Return the SHA1 of a file, failing if it doesn't exist.
pub(super) fn sha1_of_file(path: &Path) -> io::Result<[u8; 20]> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[0..n]),
            Err(e) => {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    return Err(e);
                }
            }
        };
    }
    Ok(hasher.digest().bytes())
}

/// Return the SHA1 of provided data.
pub(crate) fn sha1_of_data(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.digest().bytes()
}

pub(super) fn mtime_as_i64<P: AsRef<Path>>(path: P) -> io::Result<i64> {
    Ok(path
        .as_ref()
        .metadata()?
        .modified()?
        .duration_since(time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64)
}

pub(super) fn remove_files<S>(media_folder: &Path, files: &[S]) -> Result<()>
where
    S: AsRef<str> + std::fmt::Debug,
{
    if files.is_empty() {
        return Ok(());
    }

    let paths = files.iter().map(|f| media_folder.join(f.as_ref()));

    debug!("removing {:?}", files);
    remove_all(paths).map_err(|e| AnkiError::IOError {
        info: format!("removing files failed: {:?}", e),
    })
}

pub(super) struct AddedFile {
    pub fname: String,
    pub sha1: [u8; 20],
    pub mtime: i64,
    pub renamed_from: Option<String>,
}

/// Add a file received from AnkiWeb into the media folder.
///
/// Because AnkiWeb did not previously enforce file name limits and invalid
/// characters, we'll need to rename the file if it is not valid.
pub(super) fn add_file_from_ankiweb(
    media_folder: &Path,
    fname: &str,
    data: &[u8],
) -> Result<AddedFile> {
    let sha1 = sha1_of_data(data);
    let normalized = normalize_filename(fname);

    // if the filename is already valid, we can write the file directly
    let (renamed_from, path) = if let Cow::Borrowed(_) = normalized {
        let path = media_folder.join(normalized.as_ref());
        fs::write(&path, data)?;
        (None, path)
    } else {
        debug!("non-normalized filename received {}", fname);
        // ankiweb sent us a non-normalized filename, so we'll rename it
        let new_name = add_data_to_folder_uniquely(media_folder, fname, data, sha1)?;
        (
            Some(new_name.to_string()),
            media_folder.join(new_name.as_ref()),
        )
    };

    let mtime = mtime_as_i64(path)?;

    Ok(AddedFile {
        fname: normalized.to_string(),
        sha1,
        mtime,
        renamed_from,
    })
}

pub(super) fn data_for_file(media_folder: &Path, fname: &str) -> Result<Option<Vec<u8>>> {
    let mut file = match fs::File::open(&media_folder.join(fname)) {
        Ok(file) => file,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            } else {
                return Err(e.into());
            }
        }
    };
    let mut buf = vec![];
    file.read_to_end(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(test)]
mod test {
    use crate::media::files::{
        add_data_to_folder_uniquely, add_hash_suffix_to_file_stem, normalize_filename,
        remove_files, sha1_of_data, MAX_FILENAME_LENGTH,
    };
    use std::borrow::Cow;
    use tempfile::tempdir;

    #[test]
    fn normalize() {
        assert_eq!(normalize_filename("foo.jpg"), Cow::Borrowed("foo.jpg"));
        assert_eq!(
            normalize_filename("con.jpg[]><:\"/?*^\\|\0\r\n").as_ref(),
            "con_.jpg"
        );

        let expected_stem_len = MAX_FILENAME_LENGTH - ".jpg".len();
        assert_eq!(
            normalize_filename(&format!("{}.jpg", "x".repeat(MAX_FILENAME_LENGTH * 2))),
            "x".repeat(expected_stem_len) + ".jpg"
        );
    }

    #[test]
    fn add_hash_suffix() {
        let hash = sha1_of_data("hello".as_bytes());
        assert_eq!(
            add_hash_suffix_to_file_stem("test.jpg", &hash).as_str(),
            "test-aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d.jpg"
        );
    }

    #[test]
    fn adding_removing() {
        let dir = tempdir().unwrap();
        let dpath = dir.path();

        // no existing file case
        let h1 = sha1_of_data("hello".as_bytes());
        assert_eq!(
            add_data_to_folder_uniquely(dpath, "test.mp3", "hello".as_bytes(), h1).unwrap(),
            "test.mp3"
        );

        // same contents case
        assert_eq!(
            add_data_to_folder_uniquely(dpath, "test.mp3", "hello".as_bytes(), h1).unwrap(),
            "test.mp3"
        );

        // different contents
        let h2 = sha1_of_data("hello1".as_bytes());
        assert_eq!(
            add_data_to_folder_uniquely(dpath, "test.mp3", "hello1".as_bytes(), h2).unwrap(),
            "test-88fdd585121a4ccb3d1540527aee53a77c77abb8.mp3"
        );

        let mut written_files = std::fs::read_dir(dpath)
            .unwrap()
            .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        written_files.sort();
        assert_eq!(
            written_files,
            vec![
                "test-88fdd585121a4ccb3d1540527aee53a77c77abb8.mp3",
                "test.mp3",
            ]
        );

        // remove
        remove_files(dpath, written_files.as_slice()).unwrap();
    }
}
