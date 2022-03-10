use std::fs::{read_dir, remove_file, File};
use std::io::{self, prelude::*, Read, SeekFrom};
use std::path::{Path, PathBuf};

use log::error;
use rev_buf_reader::RevBufReader;
use snap::write::FrameEncoder;

use crate::error::Error;

/// Get the path to the log file of a task.
pub fn get_log_path(task_id: usize, path: &Path) -> PathBuf {
    let task_log_dir = path.join("task_logs");
    let path = task_log_dir.join(format!("{task_id}.log"));
    path
}

/// Create and return the two file handles for the `(stdout, stderr)` log file of a task.
/// These are two handles to the same file.
pub fn create_log_file_handles(task_id: usize, path: &Path) -> Result<(File, File), Error> {
    let log_path = get_log_path(task_id, path);
    let stdout_handle = File::create(&log_path)
        .map_err(|err| Error::IoPathError(log_path, "getting stdout handle", err))?;
    let stderr_handle = stdout_handle
        .try_clone()
        .map_err(|err| Error::IoError("cloning stderr handle".to_string(), err))?;

    Ok((stdout_handle, stderr_handle))
}

/// Return the file handle for the log file of a task.
pub fn get_log_file_handle(task_id: usize, path: &Path) -> Result<File, Error> {
    let path = get_log_path(task_id, path);
    let handle = File::open(&path)
        .map_err(|err| Error::IoPathError(path, "getting log file handle", err))?;

    Ok(handle)
}

/// Remove the the log files of a task.
pub fn clean_log_handles(task_id: usize, path: &Path) {
    let path = get_log_path(task_id, path);
    if path.exists() {
        if let Err(err) = remove_file(path) {
            error!("Failed to remove stdout file for task {task_id} with error {err:?}");
        };
    }
}

/// Return the output of a task. \
/// Task output is compressed using [snap] to save some memory and bandwidth.
pub fn read_and_compress_log_file(
    task_id: usize,
    path: &Path,
    lines: Option<usize>,
) -> Result<Vec<u8>, Error> {
    let mut file = get_log_file_handle(task_id, path)?;

    let mut content = Vec::new();

    // Move the cursor to the last few lines of both files.
    if let Some(lines) = lines {
        seek_to_last_lines(&mut file, lines)?;
    }

    // Compress the full log input and pipe it into the snappy compressor
    {
        let mut compressor = FrameEncoder::new(&mut content);
        io::copy(&mut file, &mut compressor)
            .map_err(|err| Error::IoError("compressing log output".to_string(), err))?;
    }

    Ok(content)
}

/// Return the last lines of of a task's output. \
/// This output is uncompressed and may take a lot of memory, which is why we only read
/// the last few lines.
pub fn read_last_log_file_lines(
    task_id: usize,
    path: &Path,
    lines: usize,
) -> Result<String, Error> {
    let mut file = get_log_file_handle(task_id, path)?;

    // Get the last few lines of both files
    Ok(read_last_lines(&mut file, lines))
}

/// Remove all files in the log directory.
pub fn reset_task_log_directory(path: &Path) -> Result<(), Error> {
    let task_log_dir = path.join("task_logs");

    let files = read_dir(&task_log_dir)
        .map_err(|err| Error::IoPathError(task_log_dir, "reading task log files", err))?;

    for file in files.flatten() {
        if let Err(err) = remove_file(file.path()) {
            error!("Failed to delete log file: {err}");
        }
    }

    Ok(())
}

/// Read the last `amount` lines of a file to a string.
///
/// Only use this for logic that doesn't stream from daemon to client!
/// For streaming logic use the `seek_to_last_lines` and compress any data.
// We allow this clippy check.
// The iterators cannot be chained, as RevBufReader.lines doesn't implement the necessary traits.
#[allow(clippy::needless_collect)]
pub fn read_last_lines(file: &mut File, amount: usize) -> String {
    let reader = RevBufReader::new(file);

    let lines: Vec<String> = reader
        .lines()
        .take(amount)
        .map(|line| line.unwrap_or_else(|_| "Pueue: Failed to read line.".to_string()))
        .collect();

    lines.into_iter().rev().collect::<Vec<String>>().join("\n")
}

/// Seek the cursor of the current file to the beginning of the line that's located `amount` newlines
/// from the back of the file.
pub fn seek_to_last_lines(file: &mut File, amount: usize) -> Result<(), Error> {
    let mut reader = RevBufReader::new(file);
    // The position from which the RevBufReader starts reading.
    // The file size might change while we're reading the file. Hence we have to save it now.
    let start_position = reader
        .get_mut()
        .seek(SeekFrom::Current(0))
        .map_err(|err| Error::IoError("seeking to start of file".to_string(), err))?;
    let start_position: i64 = start_position.try_into().map_err(|_| {
        Error::Generic("Failed to convert start cursor position to i64".to_string())
    })?;

    let mut total_read_bytes: i64 = 0;
    let mut found_lines = 0;

    // Read in 4KB chunks until there's either nothing left or we found `amount` newline characters.
    'outer: loop {
        let mut buffer = vec![0; 4096];
        let read_bytes = reader
            .read(&mut buffer)
            .map_err(|err| Error::IoError("reading next log chunk".to_string(), err))?;

        // Return if there's nothing left to read.
        // We hit the start of the file and read fewer lines then specified.
        if read_bytes == 0 {
            break;
        }

        // Check each byte for a newline.
        // Even though the RevBufReader reads from behind, the bytes in the buffer are still in forward
        // order. Since we want to scan from the back, we have to reverse the buffer
        for byte in buffer[0..read_bytes].iter().rev() {
            total_read_bytes += 1;
            if *byte != b'\n' {
                continue;
            }

            // We found a newline.
            found_lines += 1;

            // We haven't visited the requested amount of lines yet.
            if found_lines != amount + 1 {
                continue;
            }

            // The RevBufReader most likely already went past this point.
            // That's why we have to set the cursor to the position of the last newline.
            // Calculate the distance from the start to the desired location.
            let distance_to_file_start = start_position - total_read_bytes + 1;
            // Cast it to u64. If it somehow became negative, just seek to the start of the
            // file.
            let distance_to_file_start: u64 = distance_to_file_start.try_into().unwrap_or(0);

            // We can safely unwrap `start_position`, as we previously casted it from an u64.
            if distance_to_file_start < start_position.try_into().unwrap() {
                // Seek to the position.
                let file = reader.get_mut();
                file.seek(SeekFrom::Start(distance_to_file_start))
                    .map_err(|err| {
                        Error::IoError("seeking to correct position".to_string(), err)
                    })?;
            }

            break 'outer;
        }
    }

    Ok(())
}
