use std::{
    cell::RefCell,
    fs::File,
    io::{Cursor, Read},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use apex_hardware::FrameBuffer;
use embedded_graphics::{
    image::{Image, ImageRaw},
    pixelcolor::BinaryColor,
    prelude::Point,
    Drawable,
};
use image::{AnimationDecoder, DynamicImage};

static GIF_MISSING: &[u8] = include_bytes!("./../../assets/gif_missing.gif");
static DISPLAY_HEIGHT: i32 = 40;
static DISPLAY_WIDTH: i32 = 128;

pub struct ImageRenderer {
    stop: Point,
    origin: Point,
    decoded_frames: Vec<Vec<u8>>,
    current_frame: AtomicUsize,
    delays: Vec<u16>,
    time_frame_last_update: Rc<RefCell<Instant>>,
}

impl ImageRenderer {
    pub fn read_image(
        image: &image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
        image_height: i32,
        image_width: i32,
        dither: bool,
    ) -> Vec<u8> {
        // Floyd–Steinberg error-diffusion dithering.
        //
        // The OLED panel is strictly 1-bit (one bit per pixel, MSB-first per
        // row — see gamesense-sdk json-handlers-screen.md). There is no
        // grayscale on the wire, so multi-tone images can only fake shades
        // through pixel density. A single global median threshold (the old
        // approach) flattens mid-tones: every pixel of a grey area lands on
        // one side of the cut and the shade is lost entirely.
        //
        // Error diffusion instead quantizes each pixel against a local,
        // evolving threshold: after snapping a pixel to black or white, the
        // quantization error is distributed to its not-yet-visited neighbors
        // (right 7/16, bottom-left 3/16, bottom 5/16, bottom-right 1/16).
        // Bright regions end up with ~their brightness fraction of lit
        // pixels scattered in a fine pattern that the eye averages back
        // into a perceived shade — the same trick SteelSeries GG uses when
        // importing GIFs on Windows.

        let height = image.height();
        let width = image.width();

        // Step 1: build a float luminance plane, compositing alpha over
        // black (the panel's background is black, so transparent pixels
        // contribute zero light rather than their raw RGB).
        let mut luma = vec![0f32; (image_width * image_height) as usize];
        for y in 0..image_height {
            if y >= height as i32 || y >= DISPLAY_HEIGHT {
                continue;
            }
            for x in 0..image_width {
                if x >= width as i32 || x >= DISPLAY_WIDTH {
                    continue;
                }
                let p = image.get_pixel(x as u32, y as u32);
                let r = u32::from(p[0]);
                let g = u32::from(p[1]);
                let b = u32::from(p[2]);
                // Rec.601-ish luma approximation (same weighting family as
                // the old mean, but properly scaled to 0..255).
                let lum = (r * 299 + g * 587 + b * 114) / 1000;
                let alpha = u32::from(p[3]);
                // Composite over black.
                let composited = lum * alpha / 255;
                luma[(y * image_width + x) as usize] = composited as f32;
            }
        }

        // Step 2: serpentine-free (raster order) Floyd–Steinberg. Threshold
        // is 128 (half of full brightness); everything below pushes its
        // residual rightward/downward so local density tracks local tone.
        const THRESHOLD: f32 = 128.0;

        let mut frame_data = Vec::new();
        let mut buf: u8 = 0;

        for y in 0..image_height {
            let row_in_bounds = y < height as i32 && y < DISPLAY_HEIGHT;
            for x in 0..image_width {
                let col_in_bounds = x < width as i32 && x < DISPLAY_WIDTH;

                let old = if row_in_bounds && col_in_bounds {
                    luma[(y * image_width + x) as usize]
                } else {
                    0.0
                };

                let new = if old >= THRESHOLD { 255.0 } else { 0.0 };
                let err = old - new;

                if new > 0.0 {
                    let shift = x % 8;
                    buf += 128 >> shift;
                }

                if dither && col_in_bounds && row_in_bounds {
                    // Distribute error to neighbors (Floyd-Steinberg). When
                    // `dither` is off, this is skipped: pixels snap against
                    // a flat 128 threshold — hard-edged logos and pixel art
                    // often look crisper that way.
                    let mut push = |dx: i32, dy: i32, factor: f32| {
                        let nx = x + dx;
                        let ny = y + dy;
                        if nx < 0 || ny < 0 || nx >= image_width || ny >= image_height {
                            return;
                        }
                        let idx = (ny * image_width + nx) as usize;
                        luma[idx] += err * factor;
                    };
                    push(1, 0, 7.0 / 16.0); // right
                    push(-1, 1, 3.0 / 16.0); // bottom-left
                    push(0, 1, 5.0 / 16.0); // bottom
                    push(1, 1, 1.0 / 16.0); // bottom-right
                }

                //since we're using an array of u8, every 8 bit we need to start with a new int
                if x % 8 == 7 {
                    frame_data.push(buf);
                    buf = 0;
                }
            }
            // Row-end flush: only needed when the screen width isn't a
            // multiple of 8 (the inner loop already flushed complete bytes
            // at every x%8==7). For our 128px panel this never fires; it
            // exists so a future odd-width display still packs correctly.
            if image_width % 8 != 0 {
                frame_data.push(buf);
            }
            buf = 0;
        }
        frame_data
    }

    pub fn fit_image(image: DynamicImage, size: Point) -> DynamicImage {
        if image.height() > size.y as u32 {
            let width = image.width() * size.y as u32 / image.height();
            let height = size.y as u32;

            image.resize(width, height, image::imageops::FilterType::Nearest)
        } else if image.width() > size.x as u32 {
            let width = size.x as u32;
            let height = image.height() * size.x as u32 / image.width();

            image.resize(width, height, image::imageops::FilterType::Nearest)
        } else {
            image
        }
    }

    pub fn read_dynamic_image(
        origin: Point,
        stop: Point,
        image: DynamicImage,
        buffer: &[u8],
        dither: bool,
    ) -> Self {
        //we first get the dimension of the image
        let image_height = stop.y - origin.y;
        let image_width = stop.x - origin.x;

        let mut decoded_frames = Vec::new();
        let mut delays = Vec::new();

        if let Ok(gif) = image::codecs::gif::GifDecoder::new(Cursor::new(buffer)) {
            //if the image is a gif
            //NOTE we do not check for the size of each frame!
            //We can avoid doing so since we have the Self::fit_image which will resize the
            // frames correctly.

            //we go through each frame
            for frame in gif.into_frames() {
                //TODO we do not handle if the frame isn't formatted properly!
                if let Ok(frame) = frame {
                    //TODO some gifs do not have delays embedded, we should use a 100 ms in that
                    // case
                    let frame_delay: Duration = frame.delay().into();
                    let delay_ms = frame_delay.as_millis().min(u128::from(u16::MAX)) as u16;
                    delays.push(delay_ms);
                    let resized = Self::fit_image(
                        DynamicImage::ImageRgba8(frame.into_buffer()),
                        Point::new(DISPLAY_WIDTH, DISPLAY_HEIGHT),
                    );

                    decoded_frames.push(Self::read_image(
                        &resized.into_rgba8(),
                        image_height,
                        image_width,
                        dither,
                    ));
                }
            }
        } else {
            let resized = Self::fit_image(image, Point::new(DISPLAY_WIDTH, DISPLAY_HEIGHT));
            //if the image is a still image
            decoded_frames.push(Self::read_image(
                &resized.into_rgba8(),
                image_height,
                image_width,
                dither,
            ));
            delays.push(500); // Add a default delay of 500ms for single image
                              // rendering
        }

        Self {
            stop,
            origin,
            decoded_frames,
            current_frame: AtomicUsize::new(0),
            delays,
            time_frame_last_update: Rc::new(RefCell::new(Instant::now())),
        }
    }

    pub fn new(origin: Point, stop: Point, mut file: File, dither: bool) -> Self {
        let mut buffer = Vec::new();
        if let Ok(_) = file.read_to_end(&mut buffer) {
            if let Ok(image) = image::load_from_memory(&buffer) {
                Self::read_dynamic_image(origin, stop, image, &buffer, dither)
            } else {
                log::error!("Failed to decode the image.");
                Self::new_error(origin, stop, dither)
            }
        } else {
            log::error!("Failed to read the image file.");
            Self::new_error(origin, stop, dither)
        }
    }

    pub fn new_error(origin: Point, stop: Point, dither: bool) -> Self {
        Self::new_u8(origin, stop, GIF_MISSING, dither)
    }

    pub fn new_u8(origin: Point, stop: Point, u8_array: &[u8], dither: bool) -> Self {
        if let Ok(image) = image::load_from_memory(u8_array) {
            Self::read_dynamic_image(origin, stop, image, u8_array, dither)
        } else {
            log::error!("Failed to decode the image.");
            Self::new_error(origin, stop, dither)
        }
    }

    pub fn draw(&self, target: &mut FrameBuffer) -> bool {
        let frame = self.current_frame.load(Ordering::Relaxed);

        //get the data for the specified frame
        let frame_data = &self.decoded_frames[frame];

        //convert the data to an ImageRaw
        let raw_image_frame =
            ImageRaw::<BinaryColor>::new(&frame_data, (self.stop.x - self.origin.x) as u32);

        //draw the ImageRaw on the buffer
        let _ = Image::new(&raw_image_frame, self.origin).draw(target);

        //detect if we should change the frame
        let last_display_time = self.time_frame_last_update.borrow().clone();
        let current_time = Instant::now();
        let elapsed_time = current_time - last_display_time;

        if elapsed_time >= Duration::from_millis(u64::from(self.delays[frame])) {
            //the delays in the image crate isn't in increment of 10ms compared to the gif
            // crate! before we had a *10 because of it

            //update the variable only if we update the frame
            *self.time_frame_last_update.borrow_mut() = current_time;

            //increment the current_frame using atomic operations
            let next_frame = frame + 1;

            let has_gif_ended = next_frame >= self.decoded_frames.len();
            if has_gif_ended {
                //reset to frame 0
                self.current_frame.store(0, Ordering::Relaxed);
            } else {
                self.current_frame.store(next_frame, Ordering::Relaxed);
            }
            return has_gif_ended;
        }
        false
    }
}
