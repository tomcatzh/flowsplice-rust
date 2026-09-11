#include <snappy.h>
#include <cstddef>
#include <cstdint>
#include <limits>

// Both this bridge and upstream are built without exceptions. All writable
// buffers belong to Rust; no exception can unwind across this C boundary.
extern "C" int flowsplice_snappy_length(const char* input, size_t size,
                                        size_t* output_size) noexcept {
  if (!input || !output_size) return 1;
  return snappy::GetUncompressedLength(input, size, output_size) ? 0 : 1;
}
extern "C" int flowsplice_snappy_decode(const char* input, size_t size,
                                        char* output, size_t capacity) noexcept {
  if (!input || !output) return 1;
  size_t length = 0;
  if (!snappy::GetUncompressedLength(input, size, &length) || length != capacity)
    return 1;
  return snappy::RawUncompress(input, size, output) ? 0 : 1;
}
#ifdef FLOWSPLICE_SNAPPY_ENCODE
extern "C" int flowsplice_snappy_encode(const char* input, size_t size,
                                        char* output, size_t capacity,
                                        size_t* written) noexcept {
  if (!input || !output || !written || size > UINT32_MAX ||
      size > std::numeric_limits<size_t>::max() - 32 - size / 6) return 1;
  if (capacity < snappy::MaxCompressedLength(size)) return 1;
  snappy::RawCompress(input, size, output, written, snappy::CompressionOptions{1});
  return *written <= capacity ? 0 : 1;
}
#endif
