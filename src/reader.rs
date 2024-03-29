use std::ptr;
use litcontainers::*;
use litaudio::*;
use crate::sys::*;
use crate::ffmpeg::*;
use crate::error::Error;

pub struct Input {
	format_ctx: FormatContext,
	stream: Stream,
	codec_ctx: DecoderOpen,
}

impl Input {
	pub fn open<F>(path: &str, format_picker: F) -> Result<Self, Error>
		where F: Fn(FormatIter) -> Option<SampleFormat>
	{
		let format_ctx = open_read(path)?;
		let stream = format_ctx.get_audio_stream()
			.ok_or("Could not find any audio stream in the file")?;
		let codec_params = stream.parameters();
		let mut codec_ctx = DecoderContext::create(&codec_params)?;

		let format_iter = codec_ctx.ctx().codec().formats()
			.ok_or("Could not find appropriate sample format")?;
		let sample_format = format_picker(format_iter)
			.ok_or("Could not find appropriate sample format")?;

		codec_ctx.set_request_sample_fmt(sample_format);
		let codec_ctx = DecoderOpen::open(codec_ctx)?;

		Ok(Input { format_ctx, stream, codec_ctx })
	}

	pub fn format_ctx(&self) -> &FormatContext { &self.format_ctx }

	pub fn stream(&self) -> &Stream { &self.stream }

	pub fn codec_ctx(&self) -> &DecoderOpen { &self.codec_ctx }

	pub fn estimated_sample_count(&self) -> usize {
		(self.format_ctx.get_duration() * self.codec_ctx.ctx().ctx().sample_rate() as usize) / AV_TIME_BASE as usize
	}

	pub fn channel_layout(&self) -> ChannelLayout { self.codec_ctx.ctx().ctx().channel_layout() }

	pub fn sample_format(&self) -> SampleFormat { self.codec_ctx.ctx().sample_fmt() }

	pub fn sample_rate(&self) -> i32 { self.codec_ctx.ctx().ctx().sample_rate() }

	pub fn converter(&self, dst_fmt: AudioFormat)
		-> Result<Converter, Error> {
		Converter::new(
			AudioFormat::new(self.channel_layout(), self.sample_format(), self.sample_rate()),
			dst_fmt
		)
	}
}

pub struct Reader<'a, T, P, S>
	where T: Sample, P: SamplePackingType, S: StorageMut<T> + DynamicSampleStorage<T> + StorageConstructor<T>
{
	input: Input,
	output: AudioContainer<T, P, S>,
	cursor: SliceMut<'a, T, S::Rows, S::RowStride, Dynamic, S::ColStride>,
	converter: Option<Converter>,
	sample_count: usize
}

impl<'a, T, P, S> Reader<'a, T, P, S>
	where T: Sample, P: SamplePackingType, S: StorageMut<T> + DynamicSampleStorage<T> + StorageConstructor<T>
{
	pub fn open(path: &str, channel_count: Option<usize>) -> Result<Self, Error> {
		let input = Input::open(
			&path,
			|i| pick_best_format(i, SampleFormat::from_type::<T, P>())
		)?;

		let channel_count = match (channel_count, S::Rows::try_to_usize()) {
			(None, None) => S::Rows::from_usize(input.channel_layout().channels() as usize),
			(Some(c), None) => S::Rows::from_usize(c),
			(_, Some(c)) => S::Rows::from_usize(c),
		};

		let mut output = AudioContainer::zeros(
			Size::new(channel_count, D!(input.estimated_sample_count()))
		);
		output.set_sample_rate(input.sample_rate());

		let use_converter = input.sample_format() != SampleFormat::from_type::<T, P>()
			|| channel_count.value() != input.channel_layout().channels() as usize;

		let converter = match use_converter {
			false => None,
			true => Some(input.converter(AudioFormat::from_storage(&output))?)
		};

		let cursor = SliceBase::new(
			unsafe {
				PtrStorageMut::new(
					std::ptr::null_mut(),
					Size::new(channel_count, Dynamic::new(0)),
					output.strides()
				)},
		).into();

		Ok(Reader { input, output, cursor, converter, sample_count: 0 })
	}

	pub fn read(mut self) -> Result<AudioContainer<T, P, S>, Error> {
		let mut frame = Frame::empty().unwrap();
		let mut packet = Packet::empty();

		while match self.read_frame(&mut packet, &mut frame) {
			Err(Error::FFM(FFError::Again)) => true,
			Err(Error::FFM(FFError::Eof)) => false,
			Err(e) => return Err(e),
			Ok(_) => true
		} {}

		self.output.set_samples(self.sample_count);
		Ok(self.output)
	}

	fn read_frame(&mut self, packet: &mut Packet, frame: &mut Frame) -> Result<(), Error> {
		packet.read(&self.input.format_ctx())?;

		if packet.stream_id() != self.input.stream().id() {
			packet.reset();
			return Ok(());
		}

		match packet.send(&self.input.codec_ctx()) {
			Err(Error::FFM(FFError::Again)) => {},
			Err(e) => return Err(e),
			_ => {}
		}

		while match frame.recieve(&self.input.codec_ctx()) {
			Err(Error::FFM(FFError::Again)) => false,
			Err(e) => return Err(e),
			_ => true
		} {
			if self.output.samples() < self.sample_count + frame.nb_samples() as usize {
				self.output.set_samples(self.sample_count + frame.nb_samples() as usize);
			}

			let buffer_size = self.output.samples() - self.sample_count;
			self.cursor.storage_mut().storage_mut().shift_col_to(&mut self.output, self.sample_count, buffer_size);

			self.copy_frame_to_cursor(frame)?;

			self.sample_count += frame.nb_samples() as usize;
		}

		Ok(())
	}

	pub fn copy_frame_to_cursor(&mut self, frame: &mut Frame) -> Result<(), Error> {
		match self.converter {
			None => {
				match self.output.packing_type() {
					SamplePacking::Interleaved => {
						unsafe {
							ptr::copy_nonoverlapping(
								frame.data_ptr(0) as *const T,
								self.cursor.as_row_ptr_mut(0),
								(frame.nb_samples() as usize) * self.cursor.rows()
							);
						}
					},
					SamplePacking::Deinterleaved => {
						for c in 0..self.cursor.rows() {
							unsafe {
								ptr::copy_nonoverlapping(
									frame.data_ptr(c) as *const T,
									self.cursor.as_row_ptr_mut(c),
									frame.nb_samples() as usize
								);
							}
						}
					}
				}
			},
			Some(ref mut converter) => {
				converter.convert_frame(frame, &mut self.cursor)?;
			}
		}
		Ok(())
	}
}

