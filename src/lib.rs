extern crate bit_vec;
extern crate byteorder;
extern crate crc;
extern crate libc;
extern crate libz_sys;
extern crate num_cpus;
extern crate scoped_pool;

use scoped_pool::Pool;
use std::collections::{HashMap, HashSet};
use std::fs::{File, copy};
use std::io::{BufWriter, Write, stderr, stdout};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub mod deflate {
    pub mod deflate;
    pub mod stream;
}
pub mod png;

#[derive(Clone,Debug)]
/// Options controlling the output of the `optimize` function
pub struct Options {
    /// Whether the input file should be backed up before writing the output
    pub backup: bool,
    /// Path to write the output file to
    pub out_file: PathBuf,
    /// Used only in CLI interface
    pub out_dir: Option<PathBuf>,
    /// Write to stdout instead of a file
    pub stdout: bool,
    /// Attempt to fix errors when decoding the input file
    pub fix_errors: bool,
    /// Don't actually write any output, just calculate the best results
    pub pretend: bool,
    /// Used only in CLI interface
    pub recursive: bool,
    /// Overwrite existing output files
    pub clobber: bool,
    /// Create new output files if they don't exist
    pub create: bool,
    /// Write to output even if there was no improvement in compression
    pub force: bool,
    /// Ensure the output file has the same permissions as the input file
    pub preserve_attrs: bool,
    /// How verbose the console logging should be (`None` for quiet, `Some(0)` for normal, `Some(1)` for verbose)
    pub verbosity: Option<u8>,
    /// Which filters to try on the file (0-5)
    pub filter: HashSet<u8>,
    /// Whether to change the interlacing type of the file
    /// `None` will not change the current interlacing type
    /// `Some(x)` will change the file to interlacing mode `x`
    pub interlace: Option<u8>,
    /// Which zlib compression levels to try on the file (1-9)
    pub compression: HashSet<u8>,
    /// Which zlib memory levels to try on the file (1-9)
    pub memory: HashSet<u8>,
    /// Which zlib compression strategies to try on the file (0-3)
    pub strategies: HashSet<u8>,
    /// Window size to use when compressing the file, as `2^window` bytes
    /// Doesn't affect compression but may affect speed and memory usage
    /// 15 is recommended default, 8-15 are valid values
    pub window: u8,
    /// Whether to attempt bit depth reduction
    pub bit_depth_reduction: bool,
    /// Whether to attempt color type reduction
    pub color_type_reduction: bool,
    /// Whether to attempt palette reduction
    pub palette_reduction: bool,
    /// Whether to perform IDAT recoding
    /// If any type of reduction is performed, IDAT recoding will be performed
    /// regardless of this setting
    pub idat_recoding: bool,
    /// Which headers to strip from the PNG file, if any
    pub strip: png::Headers,
    /// Whether to use heuristics to pick the best filter and compression
    /// Intended for use with `-o 1` from the CLI interface
    pub use_heuristics: bool,
}

/// Perform optimization on the input file using the options provided
pub fn optimize(filepath: &Path, opts: &Options) -> Result<(), String> {
    type TrialWithData = (u8, u8, u8, u8, Vec<u8>);

    // Decode PNG from file
    if opts.verbosity.is_some() {
        writeln!(&mut stderr(), "Processing: {}", filepath.to_str().unwrap()).ok();
    }
    let in_file = Path::new(filepath);
    let mut png = match png::PngData::new(&in_file) {
        Ok(x) => x,
        Err(x) => return Err(x),
    };

    // Print png info
    let idat_original_size = png.idat_data.len();
    let file_original_size = filepath.metadata().unwrap().len() as usize;
    if opts.verbosity.is_some() {
        writeln!(&mut stderr(),
                 "    {}x{} pixels, PNG format",
                 png.ihdr_data.width,
                 png.ihdr_data.height)
            .ok();
        if let Some(palette) = png.palette.clone() {
            writeln!(&mut stderr(),
                     "    {} bits/pixel, {} colors in palette",
                     png.ihdr_data.bit_depth,
                     palette.len() / 3)
                .ok();
        } else {
            writeln!(&mut stderr(),
                     "    {}x{} bits/pixel, {:?}",
                     png.channels_per_pixel(),
                     png.ihdr_data.bit_depth,
                     png.ihdr_data.color_type)
                .ok();
        }
        writeln!(&mut stderr(),
                 "    IDAT size = {} bytes",
                 idat_original_size)
            .ok();
        writeln!(&mut stderr(),
                 "    File size = {} bytes",
                 file_original_size)
            .ok();
    }

    let mut filter = opts.filter.clone();
    let compression = opts.compression.clone();
    let memory = opts.memory.clone();
    let mut strategies = opts.strategies.clone();

    if opts.use_heuristics {
        // Heuristically determine which set of options to use
        if png.ihdr_data.bit_depth.as_u8() >= 8 &&
           png.ihdr_data.color_type != png::ColorType::Indexed {
            if filter.is_empty() {
                filter.insert(5);
            }
            if strategies.is_empty() {
                strategies.insert(1);
            }
        } else {
            if filter.is_empty() {
                filter.insert(0);
            }
            if strategies.is_empty() {
                strategies.insert(0);
            }
        }
    }

    let mut something_changed = false;

    if opts.bit_depth_reduction {
        if png.reduce_bit_depth() {
            something_changed = true;
            if opts.verbosity == Some(1) {
                report_reduction(&png);
            }
        };
    }

    if opts.color_type_reduction {
        if png.reduce_color_type() {
            something_changed = true;
            if opts.verbosity == Some(1) {
                report_reduction(&png);
            }
        };
    }

    if opts.palette_reduction {
        if png.reduce_palette() {
            something_changed = true;
            if opts.verbosity == Some(1) {
                report_reduction(&png);
            }
        };
    }

    if something_changed && opts.verbosity.is_some() {
        report_reduction(&png);
    }

    if let Some(interlacing) = opts.interlace {
        if png.change_interlacing(interlacing) {
            something_changed = true;
        }
    }

    if opts.idat_recoding || something_changed {
        // Use 1 thread on single-core, otherwise use threads = 1.5x CPU cores
        let num_cpus = num_cpus::get();
        let thread_count = num_cpus + (num_cpus >> 1);
        let pool = Pool::new(thread_count);
        // Go through selected permutations and determine the best
        let best: Arc<Mutex<Option<TrialWithData>>> = Arc::new(Mutex::new(None));
        let combinations = filter.len() * compression.len() * memory.len() * strategies.len();
        let mut results: Vec<(u8, u8, u8, u8)> = Vec::with_capacity(combinations);
        let mut filters: HashMap<u8, Vec<u8>> = HashMap::with_capacity(filter.len());
        if opts.verbosity.is_some() {
            writeln!(&mut stderr(), "Trying: {} combinations", combinations).ok();
        }

        for f in &filter {
            let filtered = png.filter_image(*f);
            filters.insert(*f, filtered.clone());
            for zc in &compression {
                for zm in &memory {
                    for zs in &strategies {
                        results.push((*f, *zc, *zm, *zs));
                    }
                }
            }
        }
        pool.scoped(|scope| {
            let original_len = png.idat_data.len();
            let interlacing_changed = opts.interlace.is_some() &&
                                      opts.interlace != Some(png.ihdr_data.interlaced);
            for trial in &results {
                let filtered = filters.get(&trial.0).unwrap();
                let best = best.clone();
                scope.execute(move || {
                    let new_idat = deflate::deflate::deflate(filtered,
                                                             trial.1,
                                                             trial.2,
                                                             trial.3,
                                                             opts.window)
                                       .unwrap();

                    if opts.verbosity == Some(1) {
                        writeln!(&mut stderr(),
                                 "    zc = {}  zm = {}  zs = {}  f = {}        {} bytes",
                                 trial.1,
                                 trial.2,
                                 trial.3,
                                 trial.0,
                                 new_idat.len())
                            .ok();
                    }

                    let mut best = best.lock().unwrap();
                    if (best.is_some() &&
                        new_idat.len() < best.as_ref().map(|x| x.4.len()).unwrap()) ||
                       (best.is_none() &&
                        (new_idat.len() < original_len || interlacing_changed || opts.force)) {
                        *best = Some((trial.0, trial.1, trial.2, trial.3, new_idat));
                    }
                });
            }
        });

        let mut final_best = best.lock().unwrap();
        if let Some(better) = final_best.take() {
            png.idat_data = better.4.clone();
            if opts.verbosity.is_some() {
                writeln!(&mut stderr(), "Found better combination:").ok();
                writeln!(&mut stderr(),
                         "    zc = {}  zm = {}  zs = {}  f = {}        {} bytes",
                         better.1,
                         better.2,
                         better.3,
                         better.0,
                         png.idat_data.len())
                    .ok();
            }
        }
    }

    match opts.strip.clone() {
        // Strip headers
        png::Headers::None => (),
        png::Headers::Some(hdrs) => {
            for hdr in &hdrs {
                png.aux_headers.remove(hdr);
            }
        }
        png::Headers::Safe => {
            const PRESERVED_HEADERS: [&'static str; 9] = ["cHRM", "gAMA", "iCCP", "sBIT", "sRGB",
                                                          "bKGD", "hIST", "pHYs", "sPLT"];
            let mut preserved = HashMap::new();
            for (hdr, contents) in &png.aux_headers {
                if PRESERVED_HEADERS.contains(&hdr.as_ref()) {
                    preserved.insert(hdr.clone(), contents.clone());
                }
            }
            png.aux_headers = preserved;
        }
        png::Headers::All => {
            png.aux_headers = HashMap::new();
        }
    }

    let output_data = png.output();
    if file_original_size <= output_data.len() && !opts.force && opts.interlace.is_none() {
        writeln!(&mut stderr(), "File already optimized").ok();
        return Ok(());
    }

    if opts.pretend {
        writeln!(&mut stderr(), "Running in pretend mode, no output").ok();
    } else {
        if opts.backup {
            match copy(in_file,
                       in_file.with_extension(format!("bak.{}",
                                                      in_file.extension()
                                                             .unwrap()
                                                             .to_str()
                                                             .unwrap()))) {
                Ok(x) => x,
                Err(_) => {
                    return Err(format!("Unable to write to backup file at {}",
                                       opts.out_file.display()))
                }
            };
        }

        if opts.stdout {
            let mut buffer = BufWriter::new(stdout());
            match buffer.write_all(&output_data) {
                Ok(_) => (),
                Err(_) => return Err("Unable to write to stdout".to_owned()),
            }
        } else {
            let out_file = match File::create(&opts.out_file) {
                Ok(x) => x,
                Err(_) => {
                    return Err(format!("Unable to write to file {}", opts.out_file.display()))
                }
            };
            let mut buffer = BufWriter::new(out_file);
            match buffer.write_all(&output_data) {
                Ok(_) => {
                    if opts.verbosity.is_some() {
                        writeln!(&mut stderr(), "Output: {}", opts.out_file.display()).ok();
                    }
                }
                Err(_) => {
                    return Err(format!("Unable to write to file {}", opts.out_file.display()))
                }
            }
        }
    }
    if opts.verbosity.is_some() {
        if idat_original_size >= png.idat_data.len() {
            writeln!(&mut stderr(),
                     "    IDAT size = {} bytes ({} bytes decrease)",
                     png.idat_data.len(),
                     idat_original_size - png.idat_data.len())
                .ok();
        } else {
            writeln!(&mut stderr(),
                     "    IDAT size = {} bytes ({} bytes increase)",
                     png.idat_data.len(),
                     png.idat_data.len() - idat_original_size)
                .ok();
        }
        if file_original_size >= output_data.len() {
            writeln!(&mut stderr(),
                     "    file size = {} bytes ({} bytes = {:.2}% decrease)",
                     output_data.len(),
                     file_original_size - output_data.len(),
                     (file_original_size - output_data.len()) as f64 / file_original_size as f64 *
                     100f64)
                .ok();
        } else {
            writeln!(&mut stderr(),
                     "    file size = {} bytes ({} bytes = {:.2}% increase)",
                     output_data.len(),
                     output_data.len() - file_original_size,
                     (output_data.len() - file_original_size) as f64 / file_original_size as f64 *
                     100f64)
                .ok();
        }
    }
    Ok(())
}

fn report_reduction(png: &png::PngData) {
    if let Some(palette) = png.palette.clone() {
        writeln!(&mut stderr(),
                 "Reducing image to {} bits/pixel, {} colors in palette",
                 png.ihdr_data.bit_depth,
                 palette.len() / 3)
            .ok();
    } else {
        writeln!(&mut stderr(),
                 "Reducing image to {}x{} bits/pixel, {}",
                 png.channels_per_pixel(),
                 png.ihdr_data.bit_depth,
                 png.ihdr_data.color_type)
            .ok();
    }
}
