// Benchmark-only bridge to pinned upstream reference implementations.
#include <snappy.h>
#include <zstd.h>
#include <lz4.h>
#include <lz4hc.h>
#include <cstring>
#include <cstdlib>
#include <climits>
#include <limits>
#include <new>

struct Codec {
    int kind;
    ZSTD_CCtx* c = nullptr;
    ZSTD_DCtx* d = nullptr;
    void* hc = nullptr;
};
static constexpr size_t error = std::numeric_limits<size_t>::max();
#define STRINGIFY_IMPL(x) #x
#define STRINGIFY(x) STRINGIFY_IMPL(x)
static_assert(SNAPPY_MAJOR == 1 && SNAPPY_MINOR == 2 && SNAPPY_PATCHLEVEL == 2);
static_assert(snappy::CompressionOptions::MaxCompressionLevel() == 2);
static_assert(ZSTD_VERSION_NUMBER == 10507);
static_assert(LZ4_VERSION_NUMBER == 11000);
extern "C" {
void* bench_new(int kind) {
    auto* p = new (std::nothrow) Codec{kind};
    if (!p) return nullptr;
    if (kind == 1) {
        p->c = ZSTD_createCCtx(); p->d = ZSTD_createDCtx();
        if (!p->c || !p->d) {
            ZSTD_freeCCtx(p->c); ZSTD_freeDCtx(p->d); delete p; return nullptr;
        }
    }
    if (kind == 4) {
        p->hc = std::malloc(LZ4_sizeofStateHC());
        if (!p->hc) { delete p; return nullptr; }
    }
    return p;
}
void bench_free(void* v) {
    auto* p = static_cast<Codec*>(v);
    if (!p) return;
    ZSTD_freeCCtx(p->c); ZSTD_freeDCtx(p->d); std::free(p->hc); delete p;
}
size_t bench_bound(int kind, size_t n) {
    if (n > INT_MAX) return error;
    if (kind == 0) return n;
    if (kind == 1) return ZSTD_compressBound(n);
    if (kind == 2) return snappy::MaxCompressedLength(n);
    if (kind == 3 || kind == 4) return LZ4_compressBound(static_cast<int>(n));
    return error;
}
size_t bench_compress(void* v, int level, const char* src, size_t n,
                      char* dst, size_t capacity) {
    auto* p = static_cast<Codec*>(v);
    if (n > INT_MAX || capacity > INT_MAX || capacity < bench_bound(p->kind, n)) return error;
    switch (p->kind) {
    case 0: std::memcpy(dst, src, n); return n;
    case 1: {
        auto size = ZSTD_compressCCtx(p->c, dst, capacity, src, n, level);
        return ZSTD_isError(size) ? error : size;
    }
    case 2: {
        if (level != 1 && level != 2) return error;
        size_t size = 0;
        snappy::RawCompress(src, n, dst, &size, snappy::CompressionOptions{level});
        return size;
    }
    case 3: {
        int size = LZ4_compress_default(src, dst, static_cast<int>(n), static_cast<int>(capacity));
        return size > 0 ? static_cast<size_t>(size) : error;
    }
    case 4: {
        int size = LZ4_compress_HC_extStateHC(p->hc, src, dst, static_cast<int>(n),
                                           static_cast<int>(capacity), level);
        return size > 0 ? static_cast<size_t>(size) : error;
    }
    default: return error;
    }
}
size_t bench_decompress(void* v, const char* src, size_t n, char* dst, size_t capacity) {
    auto* p = static_cast<Codec*>(v);
    if (n > INT_MAX || capacity > INT_MAX) return error;
    switch (p->kind) {
    case 0:
        if (n != capacity) return error;
        std::memcpy(dst, src, n); return n;
    case 1: {
        auto size = ZSTD_decompressDCtx(p->d, dst, capacity, src, n);
        return ZSTD_isError(size) ? error : size;
    }
    case 2: {
        size_t expected = 0;
        if (!snappy::GetUncompressedLength(src, n, &expected) || expected != capacity) return error;
        return snappy::RawUncompress(src, n, dst) ? expected : error;
    }
    case 3: case 4: {
        int size = LZ4_decompress_safe(src, dst, static_cast<int>(n), static_cast<int>(capacity));
        return size >= 0 ? static_cast<size_t>(size) : error;
    }
    default: return error;
    }
}
const char* bench_versions() {
    return "snappy=" STRINGIFY(SNAPPY_MAJOR) "." STRINGIFY(SNAPPY_MINOR) "." STRINGIFY(SNAPPY_PATCHLEVEL)
           ";zstd=" ZSTD_VERSION_STRING ";lz4=" LZ4_VERSION_STRING;
}
}
