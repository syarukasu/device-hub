//! Draw a readable percentage at the 16-pixel Windows notification size.

use std::{mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    },
    UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO},
};

const SIZE: usize = 16;
const GLYPHS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

pub fn icon(percent: u8, charging: bool, asleep: bool) -> HICON {
    let color = if asleep {
        0xff91_9aa8
    } else if charging {
        0xff56_dbff
    } else if percent <= 20 {
        0xffff_c16b
    } else {
        0xff71_e0ac
    };
    let pixels = draw(percent.min(100), color);
    // Supply a 2x raster so Explorer can scale it for high-DPI taskbars.
    const RASTER: usize = SIZE * 2;
    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: RASTER as i32,
        biHeight: -(RASTER as i32),
        biPlanes: 1,
        biBitCount: 32,
        ..Default::default()
    };
    // SAFETY: The DIB has RASTER*RASTER BGRA pixels. CreateIconIndirect copies
    // its input bitmaps, so release both local GDI objects after the call.
    unsafe {
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bitmap.is_null() {
            return null_mut();
        }
        if bits.is_null() {
            DeleteObject(bitmap);
            return null_mut();
        }
        let raster = std::slice::from_raw_parts_mut(bits.cast::<u32>(), RASTER * RASTER);
        for (index, pixel) in raster.iter_mut().enumerate() {
            let row = index / RASTER / 2;
            let col = (index % RASTER) / 2;
            *pixel = pixels[row * SIZE + col];
        }
        let mask_bits = [0u8; RASTER * RASTER / 8];
        let mask = CreateBitmap(
            RASTER as i32,
            RASTER as i32,
            1,
            1,
            mask_bits.as_ptr().cast(),
        );
        let icon = if mask.is_null() {
            null_mut()
        } else {
            let icon = CreateIconIndirect(&ICONINFO {
                fIcon: 1,
                hbmMask: mask,
                hbmColor: bitmap,
                ..Default::default()
            });
            DeleteObject(mask);
            icon
        };
        DeleteObject(bitmap);
        icon
    }
}

fn draw(percent: u8, accent: u32) -> [u32; SIZE * SIZE] {
    let mut pixels = [0xff18_2537; SIZE * SIZE];
    for index in [0, SIZE - 1, SIZE * (SIZE - 1), SIZE * SIZE - 1] {
        pixels[index] = 0;
    }
    pixels[14 * SIZE + 2..15 * SIZE - 2].fill(accent);

    let text = percent.to_string();
    let stretch = if text.len() == 3 { 1 } else { 2 };
    let glyph_width = stretch * 3;
    let width = (glyph_width + 1) * text.len() - 1;
    let offset = (SIZE - width) / 2;
    for (index, digit) in text.bytes().enumerate() {
        let left = offset + index * (glyph_width + 1);
        for (row, mask) in GLYPHS[usize::from(digit - b'0')].into_iter().enumerate() {
            for col in 0..3 {
                if mask & (4 >> col) == 0 {
                    continue;
                }
                for y in [2 + row * 2, 3 + row * 2] {
                    let start = y * SIZE + left + col * stretch;
                    pixels[start..start + stretch].fill(0xfff7_faff);
                }
            }
        }
    }
    pixels
}
