//! Color description that travels with every frame.
//!
//! A [`ColorSpec`] names the primaries, transfer function, Y'CbCr matrix and coded
//! range of a frame. Filters declare what they accept and produce; the graph
//! validator refuses to connect nodes whose specs disagree.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Primaries {
    Bt709,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transfer {
    /// Linear light; only used for the internal RGBA16F working space.
    Linear,
    Bt709,
    Srgb,
    /// SMPTE ST 2084. Parsed, but rejected by the graph until M3.
    Pq,
    /// ARIB STD-B67. Parsed, but rejected by the graph until M3.
    Hlg,
}

impl Transfer {
    /// Id passed to the shaders (`shaders/common.wgsl`).
    pub fn id(self) -> u32 {
        match self {
            Transfer::Linear => 0,
            Transfer::Bt709 => 1,
            Transfer::Srgb => 2,
            Transfer::Pq => 3,
            Transfer::Hlg => 4,
        }
    }

    /// Whether the shaders can evaluate this transfer today.
    pub fn is_supported(self) -> bool {
        matches!(self, Transfer::Linear | Transfer::Bt709 | Transfer::Srgb)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matrix {
    /// Identity: the frame is R'G'B', not Y'CbCr.
    Identity,
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020Ncl,
}

impl Matrix {
    /// Luma coefficients (Kr, Kb) for the Y'CbCr derivation.
    pub fn luma_coefficients(self) -> Option<(f32, f32)> {
        match self {
            Matrix::Identity => None,
            Matrix::Bt709 => Some((0.2126, 0.0722)),
            Matrix::Bt2020Ncl => Some((0.2627, 0.0593)),
        }
    }

    /// Row-major 3x3 matrix mapping (Y', Cb, Cr) with Cb/Cr centered at 0 to R'G'B'.
    pub fn ycc_to_rgb(self) -> [[f32; 3]; 3] {
        let Some((kr, kb)) = self.luma_coefficients() else {
            return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        };
        let kg = 1.0 - kr - kb;
        [
            [1.0, 0.0, 2.0 * (1.0 - kr)],
            [
                1.0,
                -2.0 * (1.0 - kb) * kb / kg,
                -2.0 * (1.0 - kr) * kr / kg,
            ],
            [1.0, 2.0 * (1.0 - kb), 0.0],
        ]
    }

    /// Row-major 3x3 matrix mapping R'G'B' to (Y', Cb, Cr) with Cb/Cr centered at 0.
    pub fn rgb_to_ycc(self) -> [[f32; 3]; 3] {
        let Some((kr, kb)) = self.luma_coefficients() else {
            return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        };
        let kg = 1.0 - kr - kb;
        [
            [kr, kg, kb],
            [-kr / (2.0 * (1.0 - kb)), -kg / (2.0 * (1.0 - kb)), 0.5],
            [0.5, -kg / (2.0 * (1.0 - kr)), -kb / (2.0 * (1.0 - kr))],
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Range {
    /// "Video" / "TV" / "MPEG" range: 16..235 luma, 16..240 chroma at 8 bits.
    Limited,
    /// "PC" / "JPEG" range: 0..255 at 8 bits.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorSpec {
    pub primaries: Primaries,
    pub transfer: Transfer,
    pub matrix: Matrix,
    pub range: Range,
}

impl Default for ColorSpec {
    /// BT.709, limited range — what nearly every SDR file is.
    fn default() -> Self {
        Self::BT709_LIMITED
    }
}

impl ColorSpec {
    pub const BT709_LIMITED: ColorSpec = ColorSpec {
        primaries: Primaries::Bt709,
        transfer: Transfer::Bt709,
        matrix: Matrix::Bt709,
        range: Range::Limited,
    };

    pub const BT709_FULL: ColorSpec = ColorSpec {
        primaries: Primaries::Bt709,
        transfer: Transfer::Bt709,
        matrix: Matrix::Bt709,
        range: Range::Full,
    };

    /// The internal working space: linear light R'G'B' (so really RGB), same primaries.
    pub fn working_space(self) -> ColorSpec {
        ColorSpec {
            primaries: self.primaries,
            transfer: Transfer::Linear,
            matrix: Matrix::Identity,
            range: Range::Full,
        }
    }

    pub fn is_rgb(self) -> bool {
        self.matrix == Matrix::Identity
    }

    /// Parse the short names accepted in a job file (`"bt709"`, `"bt709_full"`, ...).
    pub fn from_short_name(name: &str) -> Option<ColorSpec> {
        match name {
            "bt709" | "bt709_limited" => Some(Self::BT709_LIMITED),
            "bt709_full" => Some(Self::BT709_FULL),
            "bt2020_pq" => Some(ColorSpec {
                primaries: Primaries::Bt2020,
                transfer: Transfer::Pq,
                matrix: Matrix::Bt2020Ncl,
                range: Range::Limited,
            }),
            "bt2020_hlg" => Some(ColorSpec {
                primaries: Primaries::Bt2020,
                transfer: Transfer::Hlg,
                matrix: Matrix::Bt2020Ncl,
                range: Range::Limited,
            }),
            _ => None,
        }
    }

    pub fn short_name(self) -> String {
        match (self.primaries, self.transfer, self.range) {
            (Primaries::Bt709, Transfer::Bt709, Range::Limited) => "bt709".into(),
            (Primaries::Bt709, Transfer::Bt709, Range::Full) => "bt709_full".into(),
            (Primaries::Bt2020, Transfer::Pq, _) => "bt2020_pq".into(),
            (Primaries::Bt2020, Transfer::Hlg, _) => "bt2020_hlg".into(),
            _ => format!(
                "{:?}/{:?}/{:?}/{:?}",
                self.primaries, self.transfer, self.matrix, self.range
            )
            .to_lowercase(),
        }
    }
}

/// Coded-range parameters for a given bit depth, used by both shaders.
///
/// Values are in the *normalized texture* domain (0.0..=1.0), which is how the GPU
/// sees an `R8Unorm` or `R16Unorm` sample. For P010 the 10-bit code lives in the top
/// bits of a 16-bit word, so normalization is `code * 64 / 65535`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodedRange {
    /// Luma black level, normalized.
    pub y_offset: f32,
    /// Multiply `(y - y_offset)` by this to get Y' in 0..1.
    pub y_scale: f32,
    /// Chroma zero level, normalized.
    pub c_offset: f32,
    /// Multiply `(c - c_offset)` by this to get Cb/Cr in -0.5..0.5.
    pub c_scale: f32,
    /// Encoder side: integer code = round(v * mul + add), clamped to max_code.
    pub enc_y_mul: f32,
    pub enc_y_add: f32,
    pub enc_c_mul: f32,
    pub enc_c_add: f32,
    pub max_code: f32,
    /// Left shift applied to the integer code when stored (6 for P010, 0 otherwise).
    pub shift: u32,
}

impl CodedRange {
    /// `bits` is the *coded* depth (8 or 10). `word_bits` is the storage word (8 or 16).
    pub fn new(range: Range, bits: u32, word_bits: u32) -> CodedRange {
        let max_code = ((1u32 << bits) - 1) as f32;
        let word_max = ((1u32 << word_bits) - 1) as f32;
        let shift = word_bits - bits;
        let unit = (1u32 << shift) as f32 / word_max; // normalized value of code 1
        let (y_black, y_span, c_mid, c_span) = match range {
            Range::Limited => {
                let s = (1u32 << (bits - 8)) as f32;
                (16.0 * s, 219.0 * s, 128.0 * s, 224.0 * s)
            }
            Range::Full => (0.0, max_code, (1u32 << (bits - 1)) as f32, max_code),
        };
        CodedRange {
            y_offset: y_black * unit,
            y_scale: 1.0 / (y_span * unit),
            c_offset: c_mid * unit,
            c_scale: 1.0 / (c_span * unit),
            enc_y_mul: y_span,
            enc_y_add: y_black,
            enc_c_mul: c_span,
            enc_c_add: c_mid,
            max_code,
            shift,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_matrices_are_inverse() {
        let a = Matrix::Bt709.rgb_to_ycc();
        let b = Matrix::Bt709.ycc_to_rgb();
        for (i, row) in a.iter().enumerate() {
            for j in 0..3 {
                let s: f32 = row.iter().zip(b.iter()).map(|(x, br)| x * br[j]).sum();
                let expect = if i == j { 1.0 } else { 0.0 };
                assert!((s - expect).abs() < 1e-5, "({i},{j}) = {s}");
            }
        }
    }

    #[test]
    fn bt709_known_coefficients() {
        let m = Matrix::Bt709.ycc_to_rgb();
        assert!((m[0][2] - 1.5748).abs() < 1e-4);
        assert!((m[1][1] + 0.1873).abs() < 1e-3);
        assert!((m[1][2] + 0.4681).abs() < 1e-3);
        assert!((m[2][1] - 1.8556).abs() < 1e-4);
    }

    #[test]
    fn coded_range_8bit_limited() {
        let r = CodedRange::new(Range::Limited, 8, 8);
        assert!((r.y_offset - 16.0 / 255.0).abs() < 1e-6);
        assert!((r.y_scale - 255.0 / 219.0).abs() < 1e-5);
        assert!((r.c_offset - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(r.shift, 0);
        assert_eq!(r.max_code, 255.0);
    }

    #[test]
    fn coded_range_p010_limited() {
        let r = CodedRange::new(Range::Limited, 10, 16);
        assert_eq!(r.shift, 6);
        assert_eq!(r.max_code, 1023.0);
        assert!((r.y_offset - 64.0 * 64.0 / 65535.0).abs() < 1e-6);
        assert!((r.enc_y_add - 64.0).abs() < 1e-6);
        assert!((r.enc_y_mul - 876.0).abs() < 1e-6);
    }

    #[test]
    fn short_names_round_trip() {
        for n in ["bt709", "bt709_full", "bt2020_pq", "bt2020_hlg"] {
            let s = ColorSpec::from_short_name(n).unwrap();
            assert_eq!(s.short_name(), n);
        }
        assert!(ColorSpec::from_short_name("nonsense").is_none());
    }
}
