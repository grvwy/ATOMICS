//! A CPU rasterizer for egui output, so tests can take screenshots without a window or GPU.

use std::collections::HashMap;

use egui::{Color32, vec2};

#[derive(Default)]
pub struct Raster {
    textures: HashMap<egui::TextureId, egui::ColorImage>,
}

impl Raster {
    pub fn feed(&mut self, output: &egui::FullOutput) {
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                let egui::epaint::ImageData::Color(image) = &delta.image;
                match delta.pos {
                    None => {
                        self.textures.insert(*id, (**image).clone());
                    }
                    Some([x, y]) => {
                        if let Some(target) = self.textures.get_mut(id) {
                            for row in 0..image.size[1] {
                                let from = row * image.size[0];
                                let to = (y + row) * target.size[0] + x;
                                target.pixels[to..to + image.size[0]]
                                    .copy_from_slice(&image.pixels[from..from + image.size[0]]);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn paint(
        &self,
        ctx: &egui::Context,
        output: egui::FullOutput,
        size: [usize; 2],
        background: Color32,
    ) -> egui::ColorImage {
        let ppp = output.pixels_per_point;
        let mut image = egui::ColorImage::filled(size, background);
        for clipped in ctx.tessellate(output.shapes, ppp) {
            let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive else { continue };
            let Some(texture) = self.textures.get(&mesh.texture_id) else { continue };
            let clip = clipped.clip_rect * ppp;
            for triangle in mesh.indices.as_chunks::<3>().0 {
                let v: Vec<_> = triangle.iter().map(|&i| mesh.vertices[i as usize]).collect();
                let p = |n: usize| v[n].pos.to_vec2() * ppp;
                let (a, b, c) = (p(0), p(1), p(2));
                let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
                if area.abs() < 1e-6 {
                    continue;
                }
                let min = a.min(b).min(c).max(clip.min.to_vec2());
                let max = a.max(b).max(c).min(clip.max.to_vec2());
                for y in (min.y.floor() as i64).max(0)..(max.y.ceil() as i64).min(size[1] as i64) {
                    for x in (min.x.floor() as i64).max(0)..(max.x.ceil() as i64).min(size[0] as i64) {
                        let point = vec2(x as f32 + 0.5, y as f32 + 0.5);
                        let w0 = ((b.x - point.x) * (c.y - point.y) - (b.y - point.y) * (c.x - point.x)) / area;
                        let w1 = ((c.x - point.x) * (a.y - point.y) - (c.y - point.y) * (a.x - point.x)) / area;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }
                        let uv = v[0].uv.to_vec2() * w0 + v[1].uv.to_vec2() * w1 + v[2].uv.to_vec2() * w2;
                        let tx = ((uv.x * texture.size[0] as f32) as usize).min(texture.size[0] - 1);
                        let ty = ((uv.y * texture.size[1] as f32) as usize).min(texture.size[1] - 1);
                        let tex = texture.pixels[ty * texture.size[0] + tx].to_array();
                        let col = |n: usize| v[n].color.to_array();
                        let mix = |k: usize| col(0)[k] as f32 * w0 + col(1)[k] as f32 * w1 + col(2)[k] as f32 * w2;
                        let src: Vec<f32> = (0..4).map(|k| mix(k) * tex[k] as f32 / 255.0).collect();
                        let dst = &mut image.pixels[y as usize * size[0] + x as usize];
                        let d = dst.to_array();
                        let keep = 1.0 - src[3] / 255.0;
                        *dst = Color32::from_rgba_premultiplied(
                            (src[0] + d[0] as f32 * keep) as u8,
                            (src[1] + d[1] as f32 * keep) as u8,
                            (src[2] + d[2] as f32 * keep) as u8,
                            background.a(),
                        );
                    }
                }
            }
        }
        image
    }
}

pub fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&bytes)?;
    Ok(())
}
