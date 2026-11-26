#include <hip/hip_runtime.h>

#include <chrono>
#include <cstddef>
#include <cstdint>

namespace {

__device__ __constant__ uint32_t round_constants[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
};

__device__ uint32_t rotate_right(uint32_t value, uint32_t amount) {
    return (value >> amount) | (value << (32 - amount));
}

__device__ bool matches_zeroes(const uint32_t digest[8], unsigned int zeros) {
    const unsigned int full_bytes = zeros / 2;
    for (unsigned int i = 0; i < full_bytes; ++i) {
        const unsigned int byte =
            (digest[i / 4] >> (24 - 8 * (i % 4))) & 0xff;
        if (byte != 0) {
            return false;
        }
    }
    if (zeros % 2 != 0) {
        const unsigned int byte =
            (digest[full_bytes / 4] >> (24 - 8 * (full_bytes % 4))) & 0xff;
        return (byte & 0xf0) == 0;
    }
    return true;
}

__device__ void sha256(const unsigned char* message, unsigned int length, uint32_t hash[8]) {
    hash[0] = 0x6a09e667;
    hash[1] = 0xbb67ae85;
    hash[2] = 0x3c6ef372;
    hash[3] = 0xa54ff53a;
    hash[4] = 0x510e527f;
    hash[5] = 0x9b05688c;
    hash[6] = 0x1f83d9ab;
    hash[7] = 0x5be0cd19;

    const uint64_t bit_length = static_cast<uint64_t>(length) * 8;
    const unsigned int block_count = (length + 9 + 63) / 64;

    for (unsigned int block = 0; block < block_count; ++block) {
        uint32_t words[64] = {};
        for (unsigned int i = 0; i < 64; ++i) {
            const unsigned int position = block * 64 + i;
            unsigned int byte = 0;
            if (position < length) {
                byte = message[position];
            } else if (position == length) {
                byte = 0x80;
            } else if (position >= block_count * 64 - 8) {
                const unsigned int offset = position - (block_count * 64 - 8);
                byte = static_cast<unsigned int>((bit_length >> (56 - 8 * offset)) & 0xff);
            }
            words[i / 4] |= byte << (24 - 8 * (i % 4));
        }

        for (unsigned int i = 16; i < 64; ++i) {
            const uint32_t x = words[i - 15];
            const uint32_t y = words[i - 2];
            const uint32_t sigma0 = rotate_right(x, 7) ^ rotate_right(x, 18) ^ (x >> 3);
            const uint32_t sigma1 = rotate_right(y, 17) ^ rotate_right(y, 19) ^ (y >> 10);
            words[i] = words[i - 16] + sigma0 + words[i - 7] + sigma1;
        }

        uint32_t a = hash[0];
        uint32_t b = hash[1];
        uint32_t c = hash[2];
        uint32_t d = hash[3];
        uint32_t e = hash[4];
        uint32_t f = hash[5];
        uint32_t g = hash[6];
        uint32_t h = hash[7];

        for (unsigned int i = 0; i < 64; ++i) {
            const uint32_t sum1 = rotate_right(e, 6) ^ rotate_right(e, 11) ^ rotate_right(e, 25);
            const uint32_t choice = (e & f) ^ (~e & g);
            const uint32_t temp1 = h + sum1 + choice + round_constants[i] + words[i];
            const uint32_t sum0 = rotate_right(a, 2) ^ rotate_right(a, 13) ^ rotate_right(a, 22);
            const uint32_t majority = (a & b) ^ (a & c) ^ (b & c);
            const uint32_t temp2 = sum0 + majority;
            h = g;
            g = f;
            f = e;
            e = d + temp1;
            d = c;
            c = b;
            b = a;
            a = temp1 + temp2;
        }

        hash[0] += a;
        hash[1] += b;
        hash[2] += c;
        hash[3] += d;
        hash[4] += e;
        hash[5] += f;
        hash[6] += g;
        hash[7] += h;
    }
}

__global__ void find_nonce_kernel(
    const unsigned char* challenge,
    unsigned int challenge_length,
    unsigned int zeros,
    uint64_t base,
    uint64_t count,
    int* found,
    uint64_t* result
) {
    const uint64_t index = static_cast<uint64_t>(blockIdx.x) * blockDim.x + threadIdx.x;
    if (index >= count) {
        return;
    }

    const uint64_t nonce = base + index;
    unsigned char message[256];
    for (unsigned int i = 0; i < challenge_length; ++i) {
        message[i] = challenge[i];
    }

    unsigned char digits[20];
    unsigned int digit_count = 0;
    uint64_t remaining = nonce;
    do {
        digits[digit_count++] = static_cast<unsigned char>('0' + remaining % 10);
        remaining /= 10;
    } while (remaining != 0);
    for (unsigned int i = 0; i < digit_count; ++i) {
        message[challenge_length + i] = digits[digit_count - i - 1];
    }

    uint32_t digest[8];
    sha256(message, challenge_length + digit_count, digest);
    if (matches_zeroes(digest, zeros) && atomicCAS(found, 0, 1) == 0) {
        *result = nonce;
    }
}

}

extern "C" int s3_hip_find_nonce(
    const unsigned char* challenge,
    size_t challenge_length,
    unsigned int zeros,
    uint64_t timeout_ms,
    uint64_t* result
) {
    if (challenge_length > 235 || zeros > 64 || result == nullptr) {
        return -2;
    }

    int device_count = 0;
    hipError_t error = hipGetDeviceCount(&device_count);
    if (error != hipSuccess) {
        return static_cast<int>(error);
    }
    if (device_count < 1) {
        return -3;
    }
    error = hipSetDevice(0);
    if (error != hipSuccess) {
        return static_cast<int>(error);
    }

    unsigned char* device_challenge = nullptr;
    int* device_found = nullptr;
    uint64_t* device_result = nullptr;

    auto cleanup = [&](int status) {
        if (device_result != nullptr) {
            const hipError_t free_error = hipFree(device_result);
            if (status == 0 && free_error != hipSuccess) {
                status = static_cast<int>(free_error);
            }
        }
        if (device_found != nullptr) {
            const hipError_t free_error = hipFree(device_found);
            if (status == 0 && free_error != hipSuccess) {
                status = static_cast<int>(free_error);
            }
        }
        if (device_challenge != nullptr) {
            const hipError_t free_error = hipFree(device_challenge);
            if (status == 0 && free_error != hipSuccess) {
                status = static_cast<int>(free_error);
            }
        }
        return status;
    };

    if (challenge_length > 0) {
        error = hipMalloc(&device_challenge, challenge_length);
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }
        error = hipMemcpy(device_challenge, challenge, challenge_length, hipMemcpyHostToDevice);
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }
    }
    error = hipMalloc(&device_found, sizeof(int));
    if (error != hipSuccess) {
        return cleanup(static_cast<int>(error));
    }
    error = hipMalloc(&device_result, sizeof(uint64_t));
    if (error != hipSuccess) {
        return cleanup(static_cast<int>(error));
    }

    constexpr uint64_t batch_size = 1 << 20;
    constexpr unsigned int block_size = 256;
    uint64_t base = 0;
    const auto started = std::chrono::steady_clock::now();
    const auto max_timeout_ms =
        std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::time_point::max() - started
        )
            .count();
    const auto bounded_timeout_ms =
        timeout_ms > static_cast<uint64_t>(max_timeout_ms)
            ? max_timeout_ms
            : static_cast<std::chrono::milliseconds::rep>(timeout_ms);
    const auto deadline = started + std::chrono::milliseconds(bounded_timeout_ms);

    while (std::chrono::steady_clock::now() < deadline) {
        const uint64_t available = UINT64_MAX - base;
        const bool final_batch = available < batch_size;
        const uint64_t count = final_batch ? available + 1 : batch_size;
        error = hipMemset(device_found, 0, sizeof(int));
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }

        const dim3 grid(static_cast<unsigned int>((count + block_size - 1) / block_size));
        hipLaunchKernelGGL(
            find_nonce_kernel,
            grid,
            dim3(block_size),
            0,
            0,
            device_challenge,
            static_cast<unsigned int>(challenge_length),
            zeros,
            base,
            count,
            device_found,
            device_result
        );
        error = hipGetLastError();
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }
        error = hipDeviceSynchronize();
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }

        int found = 0;
        error = hipMemcpy(&found, device_found, sizeof(int), hipMemcpyDeviceToHost);
        if (error != hipSuccess) {
            return cleanup(static_cast<int>(error));
        }
        if (found != 0) {
            error = hipMemcpy(result, device_result, sizeof(uint64_t), hipMemcpyDeviceToHost);
            return cleanup(static_cast<int>(error));
        }
        if (final_batch) {
            return cleanup(-2);
        }
        base += count;
    }

    return cleanup(1);
}
