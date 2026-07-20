use std::error::Error;
use std::fmt;

/// A row-major output-by-input channel matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMatrix {
    output_channels: usize,
    input_channels: usize,
    coefficients: Vec<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatrixError {
    ZeroChannels,
    WrongCoefficientCount { expected: usize, actual: usize },
    NonFiniteCoefficient { index: usize },
    BufferLengthMismatch,
    FrameCountOverflow,
}

impl fmt::Display for MatrixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "channel counts must be greater than zero"),
            Self::WrongCoefficientCount { expected, actual } => {
                write!(f, "expected {expected} coefficients, received {actual}")
            }
            Self::NonFiniteCoefficient { index } => {
                write!(f, "coefficient {index} is not finite")
            }
            Self::BufferLengthMismatch => {
                write!(f, "audio buffer length does not match the matrix")
            }
            Self::FrameCountOverflow => write!(f, "frame count overflows the audio buffer length"),
        }
    }
}

impl Error for MatrixError {}

impl ChannelMatrix {
    pub fn new(
        output_channels: usize,
        input_channels: usize,
        coefficients: Vec<f32>,
    ) -> Result<Self, MatrixError> {
        if output_channels == 0 || input_channels == 0 {
            return Err(MatrixError::ZeroChannels);
        }
        let expected = output_channels
            .checked_mul(input_channels)
            .ok_or(MatrixError::FrameCountOverflow)?;
        if coefficients.len() != expected {
            return Err(MatrixError::WrongCoefficientCount {
                expected,
                actual: coefficients.len(),
            });
        }
        if let Some(index) = coefficients.iter().position(|value| !value.is_finite()) {
            return Err(MatrixError::NonFiniteCoefficient { index });
        }
        Ok(Self {
            output_channels,
            input_channels,
            coefficients,
        })
    }

    pub fn identity(channels: usize) -> Result<Self, MatrixError> {
        if channels == 0 {
            return Err(MatrixError::ZeroChannels);
        }
        let mut coefficients = vec![0.0; channels * channels];
        for channel in 0..channels {
            coefficients[channel * channels + channel] = 1.0;
        }
        Self::new(channels, channels, coefficients)
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    pub fn coefficients(&self) -> &[f32] {
        &self.coefficients
    }

    /// Mix interleaved `input` into interleaved `output` without allocating.
    ///
    /// `gain` is applied after the matrix. Existing samples in `output` are
    /// preserved and added to, allowing several routes to feed one sink.
    pub fn mix_interleaved(
        &self,
        frames: usize,
        input: &[f32],
        output: &mut [f32],
        gain: f32,
    ) -> Result<(), MatrixError> {
        let expected_input = frames
            .checked_mul(self.input_channels)
            .ok_or(MatrixError::FrameCountOverflow)?;
        let expected_output = frames
            .checked_mul(self.output_channels)
            .ok_or(MatrixError::FrameCountOverflow)?;
        if input.len() != expected_input || output.len() != expected_output || !gain.is_finite() {
            return Err(MatrixError::BufferLengthMismatch);
        }

        for frame in 0..frames {
            let input_frame = &input[frame * self.input_channels..][..self.input_channels];
            let output_frame = &mut output[frame * self.output_channels..][..self.output_channels];
            for (output_channel, output_sample) in output_frame.iter_mut().enumerate() {
                let row = &self.coefficients[output_channel * self.input_channels..]
                    [..self.input_channels];
                let mixed = row
                    .iter()
                    .zip(input_frame)
                    .map(|(coefficient, sample)| coefficient * sample)
                    .sum::<f32>();
                *output_sample += mixed * gain;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_preserves_interleaved_audio() {
        let matrix = ChannelMatrix::identity(2).unwrap();
        let input = [1.0, 2.0, 3.0, 4.0];
        let mut output = [0.0; 4];
        matrix.mix_interleaved(2, &input, &mut output, 1.0).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn matrix_downmixes_stereo_to_mono() {
        let matrix = ChannelMatrix::new(1, 2, vec![0.5, 0.5]).unwrap();
        let input = [1.0, 3.0, 2.0, 6.0];
        let mut output = [0.0; 2];
        matrix.mix_interleaved(2, &input, &mut output, 1.0).unwrap();
        assert_eq!(output, [2.0, 4.0]);
    }
}
