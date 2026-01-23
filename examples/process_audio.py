from silero_vad import load_silero_vad, read_audio, get_speech_timestamps
import sys

model = load_silero_vad()
wav = read_audio(sys.argv[1])


speech_timestamps = get_speech_timestamps(
  wav,
  model,
  threshold=0.5,
  min_speech_duration_ms=250,     # Default: filter out segments shorter than 250ms
  min_silence_duration_ms=100,    # Default: require 100ms silence to separate segments
  speech_pad_ms=0,                # No padding to match Rust behavior
  return_seconds=True,
)
print("Detected Speech Segments:")
if not speech_timestamps:
    print("Total segments: 0")
else:
    print(f"Total segments: {len(speech_timestamps)}")
    for i, ts in enumerate(speech_timestamps):
        print(f"Segment {i}: {ts['start']:.2f}s - {ts['end']:.2f}s")
        # Calculate duration
        duration = ts['end'] - ts['start']
        print(f"  Duration: {duration:.3f}s")