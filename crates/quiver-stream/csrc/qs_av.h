/* The libav pieces quiver-stream uses, behind a small C API whose structs Rust mirrors
 * (src/av.rs). Built against the system's FFmpeg headers, so libav's own struct layouts
 * never cross into Rust. */
#ifndef QS_AV_H
#define QS_AV_H

#include <stdint.h>

enum { QS_CODEC_H264 = 0, QS_CODEC_HEVC = 1, QS_CODEC_AV1 = 2 };
enum { QS_HW_SOFTWARE = 0, QS_HW_VULKAN = 1, QS_HW_CUDA = 2, QS_HW_VAAPI = 3 };
enum { QS_PIC_CPU = 0, QS_PIC_VULKAN = 1 };

typedef struct QsVkQueueFamily {
    int idx;
    int num;
    uint32_t flags;
    uint32_t video_caps;
} QsVkQueueFamily;

/* A Vulkan device the caller made, for libav to decode on. */
typedef struct QsVkDevice {
    void *get_instance_proc_addr;
    void *instance;
    void *physical_device;
    void *device;
    const char *const *instance_extensions;
    int nb_instance_extensions;
    const char *const *device_extensions;
    int nb_device_extensions;
    /* VkPhysicalDeviceFeatures2, with its pNext chain, as enabled at device creation. */
    const void *features;
    QsVkQueueFamily qf[16];
    int nb_qf;
} QsVkDevice;

typedef struct QsHwDevice QsHwDevice;
typedef struct QsDec QsDec;
typedef struct QsDemux QsDemux;

/* A decoded picture. For QS_PIC_CPU the planes are in data/linesize; for QS_PIC_VULKAN
 * use qs_pic_vk_lock. */
typedef struct QsPicture {
    void *frame; /* AVFrame *, owned */
    int kind;
    int width;
    int height;
    int64_t pts;
    /* The layout of the CPU planes (or the Vulkan image's software format). */
    int nb_planes;      /* 2: Y + interleaved CbCr; 3: Y, Cb, Cr */
    int log2_chroma_w;
    int log2_chroma_h;
    int depth;          /* bits per component */
    int bytes;          /* bytes per component: 1 or 2 */
    int shift;          /* where the bits sit in a 2-byte component (6 for P010) */
    uint8_t *data[3];
    int linesize[3];
    /* What the frame says about its colours (AVColorRange, AVColorSpace, AVColorTransferCharacteristic). */
    int color_range;
    int colorspace;
    int color_trc;
    int color_primaries;
    /* Time spent copying a hardware frame to memory, in microseconds (0 if none). */
    int64_t transfer_us;
} QsPicture;

/* One Vulkan image of a decoded frame, while locked. */
typedef struct QsVkImage {
    uint64_t image;
    int32_t format;     /* VkFormat of the image (multi-planar) */
    int32_t layout;     /* VkImageLayout, in and out */
    uint64_t access;    /* VkAccessFlags2, in and out */
    uint64_t semaphore; /* timeline VkSemaphore */
    uint64_t sem_value; /* wait for this value; signal it plus one, and write that back */
    uint32_t queue_family;
    uint32_t img_flags; /* VkImageCreateFlags */
    uint32_t usage;     /* VkImageUsageFlags */
    int nb_images;      /* more than one: the planes are separate images (unsupported) */
    int width;          /* the image's size: the coded size, at least the picture's */
    int height;
} QsVkImage;

typedef struct QsStreamInfo {
    int codec;
    int width;
    int height;
    int fps_num;
    int fps_den;
} QsStreamInfo;

typedef struct QsPacket {
    const uint8_t *data;
    int size;
    int key;
} QsPacket;

QsHwDevice *qs_hw_device_vulkan(const QsVkDevice *vk, char *err, int errlen);
QsHwDevice *qs_hw_device_create(int hw, char *err, int errlen);
void qs_hw_device_free(QsHwDevice *dev);

QsDec *qs_dec_open(int codec, QsHwDevice *dev, int threads, char *err, int errlen);
/* What decodes now: QS_HW_*, which can fall back to software after a failed hwaccel init. */
int qs_dec_backend(const QsDec *dec);
const char *qs_dec_name(const QsDec *dec);
/* 0 on success, else an AVERROR. */
int qs_dec_send(QsDec *dec, const uint8_t *data, int size, int64_t pts);
/* 1: a picture; 0: none yet; < 0: an AVERROR. */
int qs_dec_receive(QsDec *dec, QsPicture *pic);
void qs_dec_flush(QsDec *dec);
void qs_dec_close(QsDec *dec);

void qs_pic_free(QsPicture *pic);
int qs_pic_vk_lock(QsPicture *pic, QsVkImage *out);
void qs_pic_vk_unlock(QsPicture *pic, const QsVkImage *in);

QsDemux *qs_demux_open(const char *path, QsStreamInfo *info, char *err, int errlen);
int qs_demux_read(QsDemux *dmx, QsPacket *pkt);
void qs_demux_close(QsDemux *dmx);

void qs_av_quiet(int quiet);
void qs_av_strerror(int err, char *buf, int len);

#endif
