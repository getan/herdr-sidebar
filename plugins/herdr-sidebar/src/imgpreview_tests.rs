use super::*;
use image::{DynamicImage, ImageBuffer, Rgb};

fn tiny_png() -> Vec<u8> {
    let buffer: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(4, 4, Rgb([9, 10, 11]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(buffer)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("in-memory png encodes");
    bytes
}

fn tiny_image() -> DynamicImage {
    DynamicImage::ImageRgb8(ImageBuffer::from_pixel(4, 4, Rgb([9, 10, 11])))
}

#[test]
fn parses_every_override_value() {
    assert_eq!(parse_mode(Some("kitty")), ImgMode::Kitty);
    assert_eq!(parse_mode(Some("block")), ImgMode::Block);
    assert_eq!(parse_mode(Some("off")), ImgMode::Off);
    assert_eq!(parse_mode(None), ImgMode::Auto);
    assert_eq!(parse_mode(Some("kittty")), ImgMode::Auto);
}

#[test]
fn matches_image_names_case_insensitively() {
    assert!(is_image_name("photo.PNG"));
    assert!(is_image_name("clip.webp"));
    assert!(is_image_name("scan.tga"));
    assert!(!is_image_name("notes.txt"));
    assert!(!is_image_name("png"));
    assert!(!is_image_name("archive.tar.gz"));
}

#[test]
fn sniffs_strong_magics() {
    assert!(sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A]));
    assert!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]));
    assert!(sniff(b"GIF89a...."));
    let mut webp = b"RIFF1234WEBP".to_vec();
    webp.extend_from_slice(&[0; 4]);
    assert!(sniff(&webp));
    assert!(sniff(b"BM...."));
    assert!(sniff(&[0, 0, 1, 0, 0]));
    assert!(sniff(&[b'I', b'I', 42, 0]));
    assert!(sniff(&[b'M', b'M', 0, 42]));
    assert!(sniff(b"qoif...."));
    assert!(!sniff(b"hello, world"));
    assert!(!sniff(b""));
}

#[test]
fn claims_only_plausible_images() {
    let png = tiny_png();
    assert!(ImageDoc::try_new_with("shot.png", &png, ImgMode::Auto).is_some());
    assert!(ImageDoc::try_new_with("shot.png", &png, ImgMode::Kitty).is_some());
    assert!(ImageDoc::try_new_with("shot.png", &png, ImgMode::Off).is_none());
    assert!(ImageDoc::try_new_with("notes.txt", b"plain text", ImgMode::Auto).is_none());
    assert!(ImageDoc::try_new_with("blob.bin", b"BM", ImgMode::Auto).is_none());
    assert!(ImageDoc::try_new_with("a.bmp", b"BMxxxx", ImgMode::Auto).is_some());
    assert!(ImageDoc::try_new_with("empty.png", b"", ImgMode::Auto).is_none());
    let huge = vec![0xFFu8; MAX_IMAGE_BYTES + 1];
    assert!(ImageDoc::try_new_with("big.png", &huge, ImgMode::Auto).is_none());
}

#[test]
fn builds_halfblock_protocol_without_terminal_io() {
    let png = tiny_png();
    let picker = Picker::halfblocks();
    let mut claimed =
        ImageDoc::try_new_with("shot.png", &png, ImgMode::Auto).expect("png is claimed");
    let protocol = claimed
        .protocol_for_with(20, 10, &picker)
        .expect("halfblocks encode locally");
    assert!(!needs_clear(protocol));
    let again = claimed
        .protocol_for_with(20, 10, &picker)
        .expect("cached rebuild works");
    assert!(!needs_clear(again));
}

#[test]
fn kitty_protocol_needs_clearing() {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    let protocol = picker
        .new_protocol(
            tiny_image(),
            Size {
                width: 20,
                height: 10,
            },
            Resize::Fit(None),
        )
        .expect("kitty encodes locally");
    assert!(needs_clear(&protocol));
}

#[test]
fn graphics_latch_is_one_shot_without_a_tty() {
    mark_graphics_active();
    clear_graphics();
    clear_graphics();
}
