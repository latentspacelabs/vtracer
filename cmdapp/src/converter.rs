use std::path::Path;
use std::{fs::File, io::Write};
use std::time::Instant;

use super::config::{ColorMode, Config, ConverterConfig, Hierarchical};
use super::svg::SvgFile;
use fastrand::Rng;
use opencv::prelude::*;
use visioncortex::color_clusters::{KeyingAction, Runner, RunnerConfig, HIERARCHICAL_MAX};
use visioncortex::{
    approximate_circle_with_spline, Color, ColorImage, ColorName, CompoundPath, PathSimplifyMode, SegImage, PointF64
};

const NUM_UNUSED_COLOR_ITERATIONS: usize = 6;
/// The fraction of pixels in the top/bottom rows of the image that need to be transparent before
/// the entire image will be keyed.
const KEYING_THRESHOLD: f32 = 0.2;

const SMALL_CIRCLE: i32 = 12;


/// Convert an in-memory image into an in-memory SVG
pub fn convert(input_path: &Path, config: Config) -> Result<SvgFile, String> {
    let config = config.into_converter_config();
    match config.color_mode {
        ColorMode::Color => {
            println!("COLOR MODE");
            let img = read_color_image(input_path);
            color_image_to_svg(img?, config)
        }
        ColorMode::Binary => {
            println!("BINARY MODE");
            let img = read_color_image(input_path);
            binary_image_to_svg(img?, config)
        }
        ColorMode::Seg => {
            println!("SEG MODE");
            let img = read_seg_image(input_path);
            let svg = seg_image_to_svg(img?, config);
            return svg;
        }
    }
}

/// Convert an image file into svg file
pub fn convert_image_to_svg(
    input_path: &Path,
    output_path: &Path,
    config: Config,
) -> Result<(), String> {
    // let img = read_image(input_path)?;

    let svg = convert(input_path, config)?;
    write_svg(svg, output_path)
}

fn color_exists_in_image(img: &ColorImage, color: Color) -> bool {
    for y in 0..img.height {
        for x in 0..img.width {
            let pixel_color = img.get_pixel(x, y);
            if pixel_color.r == color.r && pixel_color.g == color.g && pixel_color.b == color.b {
                return true;
            }
        }
    }
    false
}

fn find_unused_color_in_image(img: &ColorImage) -> Result<Color, String> {
    let special_colors = IntoIterator::into_iter([
        Color::new(255, 0, 0),
        Color::new(0, 255, 0),
        Color::new(0, 0, 255),
        Color::new(255, 255, 0),
        Color::new(0, 255, 255),
        Color::new(255, 0, 255),
    ]);
    let rng = Rng::new();
    let random_colors =
        (0..NUM_UNUSED_COLOR_ITERATIONS).map(|_| Color::new(rng.u8(..), rng.u8(..), rng.u8(..)));
    for color in special_colors.chain(random_colors) {
        if !color_exists_in_image(img, color) {
            return Ok(color);
        }
    }
    Err(String::from(
        "unable to find unused color in image to use as key",
    ))
}

fn should_key_image(img: &ColorImage) -> bool {
    if img.width == 0 || img.height == 0 {
        return false;
    }

    // Check for transparency at several scanlines
    let threshold = ((img.width * 2) as f32 * KEYING_THRESHOLD) as usize;
    let mut num_transparent_boundary_pixels = 0;
    let y_positions = [
        0,
        img.height / 4,
        img.height / 2,
        3 * img.height / 4,
        img.height - 1,
    ];
    for y in y_positions {
        for x in 0..img.width {
            if img.get_pixel(x, y).a == 0 {
                num_transparent_boundary_pixels += 1;
            }
            if num_transparent_boundary_pixels >= threshold {
                return true;
            }
        }
    }

    false
}

fn color_image_to_svg(mut img: ColorImage, config: ConverterConfig) -> Result<SvgFile, String> {
    let width = img.width;
    let height = img.height;

    let key_color = if should_key_image(&img) {
        let key_color = find_unused_color_in_image(&img)?;
        for y in 0..height {
            for x in 0..width {
                if img.get_pixel(x, y).a == 0 {
                    img.set_pixel(x, y, &key_color);
                }
            }
        }
        key_color
    } else {
        // The default color is all zeroes, which is treated by visioncortex as a special value meaning no keying will be applied.
        Color::default()
    };

    let runner = Runner::new(
        RunnerConfig {
            diagonal: config.layer_difference == 0,
            hierarchical: HIERARCHICAL_MAX,
            batch_size: 25600,
            good_min_area: config.filter_speckle_area,
            good_max_area: (width * height),
            is_same_color_a: config.color_precision_loss,
            is_same_color_b: 1,
            deepen_diff: config.layer_difference,
            hollow_neighbours: 1,
            key_color,
            keying_action: if matches!(config.hierarchical, Hierarchical::Cutout) {
                KeyingAction::Keep
            } else {
                KeyingAction::Discard
            },
        },
        img,
    );

    let mut clusters = runner.run();
    
    match config.hierarchical {
        Hierarchical::Stacked => {}
        Hierarchical::Cutout => {
            let view = clusters.view();
            let image = view.to_color_image();
            let runner = Runner::new(
                RunnerConfig {
                    diagonal: false,
                    hierarchical: 64,
                    batch_size: 25600,
                    good_min_area: 0,
                    good_max_area: (image.width * image.height) as usize,
                    is_same_color_a: 0,
                    is_same_color_b: 1,
                    deepen_diff: 0,
                    hollow_neighbours: 0,
                    key_color,
                    keying_action: KeyingAction::Discard,
                },
                image,
            );
            clusters = runner.run();
        }
    }

    let view = clusters.view();

    // println!("{}", view.clusters_output.len());
    // println!("{}", clusters.output_len());

    let mut svg = SvgFile::new(width, height, config.path_precision);
    for &cluster_index in view.clusters_output.iter().rev() {
        let cluster = view.get_cluster(cluster_index);
        let paths = if matches!(config.mode, PathSimplifyMode::Spline)
            // && cluster.rect.width() < SMALL_CIRCLE
            // && cluster.rect.height() < SMALL_CIRCLE
            && cluster.to_shape(&view).is_circle()
        {
            let mut paths = CompoundPath::new();
            paths.add_spline(approximate_circle_with_spline(
                cluster.rect.left_top(),
                cluster.rect.width(),
            ));
            paths
        } else {
            cluster.to_compound_path(
                &view,
                false,
                config.mode,
                config.corner_threshold,
                config.length_threshold,
                config.max_iterations,
                config.splice_threshold,
                config.max_error_simp,
            )
        };
        svg.add_path(paths, cluster.residue_color());
    }

    Ok(svg)
}

fn binary_image_to_svg(img: ColorImage, config: ConverterConfig) -> Result<SvgFile, String> {
    let img = img.to_binary_image(|x| x.r < 128);
    let width = img.width;
    let height = img.height;

    let clusters = img.to_clusters(false);

    let mut svg = SvgFile::new(width, height, config.path_precision);
    for i in 0..clusters.len() {
        let cluster = clusters.get_cluster(i);
        if cluster.size() >= config.filter_speckle_area {
            let paths = cluster.to_compound_path(
                config.mode,
                config.corner_threshold,
                config.length_threshold,
                config.max_iterations,
                config.splice_threshold,
                config.max_error_simp,
            );
            svg.add_path(paths, Color::color(&ColorName::Black));
        }
    }

    Ok(svg)
}

fn seg_image_to_svg(img: SegImage, config: ConverterConfig) -> Result<SvgFile, String> {
    let width = img.width;
    let height = img.height;

    // Use a HashSet to get unique values

    let clusters = img.to_clusters();
    let mut svg = SvgFile::new(width, height, config.path_precision);
    for i in 0..clusters.len() {
        let cluster = clusters.get_cluster(i);
        println!("cluster size {}", cluster.size());
        if cluster.size() >= config.filter_speckle_area {
            let paths = cluster.to_compound_path(
                config.mode,
                config.corner_threshold,
                config.length_threshold,
                config.max_iterations,
                config.splice_threshold,
                config.max_error_simp,
            );
            // let (string, offset) = paths.to_svg_string(true, PointF64 { x: 0.0, y: 0.0 }, None);
            svg.add_path(paths, Color::color(&ColorName::Black));
        }
    }

    Ok(svg)
}

fn read_color_image(input_path: &Path) -> Result<ColorImage, String> {
    let img = image::open(input_path);
    let img = match img {
        Ok(file) => file.to_rgba8(),
        Err(_) => return Err(String::from("No image file found at specified input path")),
    };

    let (width, height) = (img.width() as usize, img.height() as usize);
    let img = ColorImage {
        pixels: img.as_raw().to_vec(),
        width,
        height,
    };

    Ok(img)
}

fn read_seg_image(input_path: &Path) -> Result<SegImage, String> {
    if let Some(path_str) = input_path.to_str() {
        let img = opencv::imgcodecs::imread(path_str, opencv::imgcodecs::IMREAD_GRAYSCALE);

        let mat = match img {
            Ok(mat) => mat,
            Err(_) => return Err(String::from("No image file found at specified input path")),
        };

        let vec: Vec<u8> = match mat.to_vec_2d() {
            Ok(vec) => vec.concat(),
            Err(_) => return Err(String::from("No image file found at specified input path")),
        };
        
        // let sum: u64 = vec.iter().map(|&x| x as u64).sum();

        // // Print the sum
        // println!("Sum: {}", sum);


        // if let Ok(s) = std::str::from_utf8(&vec) {
        //     println!("{}", s);
        // } else {
        //     println!("Invalid UTF-8 data");
        // }

        println!("Length: {}", vec.len());
    
        return Ok(SegImage::new_pixels(vec, mat.cols() as usize, mat.rows() as usize));
    } else {
        return Err(String::from("No image file found at specified input path"));
    }
}


fn write_svg(svg: SvgFile, output_path: &Path) -> Result<(), String> {
    let out_file = File::create(output_path);
    let mut out_file = match out_file {
        Ok(file) => file,
        Err(_) => return Err(String::from("Cannot create output file.")),
    };

    write!(&mut out_file, "{}", svg).expect("failed to write file.");

    Ok(())
}
