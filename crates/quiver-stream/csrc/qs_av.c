/* See qs_av.h. */
#define VK_NO_PROTOTYPES
#include <stdio.h>
#include <string.h>
#include <time.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vulkan.h>
#include <libavutil/pixdesc.h>
#include <libavutil/opt.h>

#include "qs_av.h"

struct QsHwDevice {
    AVBufferRef *ref;
    int hw;
};

struct QsDec {
    AVCodecContext *ctx;
    AVPacket *pkt;
    int hw;                       /* what was asked for */
    int active;                   /* what get_format picked */
    enum AVPixelFormat hw_pix_fmt;
};

struct QsDemux {
    AVFormatContext *fmt;
    AVPacket *pkt;
    int stream;
};

static void set_err(char *err, int errlen, const char *what, int code) {
    if (!err || errlen <= 0)
        return;
    if (code) {
        char buf[128];
        av_strerror(code, buf, sizeof(buf));
        snprintf(err, errlen, "%s: %s", what, buf);
    } else {
        snprintf(err, errlen, "%s", what);
    }
}

static int64_t now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

void qs_av_quiet(int quiet) { av_log_set_level(quiet ? AV_LOG_ERROR : AV_LOG_INFO); }

void qs_av_strerror(int err, char *buf, int len) { av_strerror(err, buf, len); }

/* --- devices ------------------------------------------------------------------------------ */

QsHwDevice *qs_hw_device_vulkan(const QsVkDevice *in, char *err, int errlen) {
    AVBufferRef *ref = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_VULKAN);
    if (!ref) {
        set_err(err, errlen, "av_hwdevice_ctx_alloc(vulkan) failed", 0);
        return NULL;
    }
    AVHWDeviceContext *hd = (AVHWDeviceContext *)ref->data;
    AVVulkanDeviceContext *vk = hd->hwctx;
    vk->get_proc_addr = (PFN_vkGetInstanceProcAddr)in->get_instance_proc_addr;
    vk->inst = (VkInstance)in->instance;
    vk->phys_dev = (VkPhysicalDevice)in->physical_device;
    vk->act_dev = (VkDevice)in->device;
    vk->device_features = *(const VkPhysicalDeviceFeatures2 *)in->features;
    vk->enabled_inst_extensions = in->instance_extensions;
    vk->nb_enabled_inst_extensions = in->nb_instance_extensions;
    vk->enabled_dev_extensions = in->device_extensions;
    vk->nb_enabled_dev_extensions = in->nb_device_extensions;
    vk->nb_qf = in->nb_qf > 16 ? 16 : in->nb_qf;
    for (int i = 0; i < vk->nb_qf; i++) {
        vk->qf[i].idx = in->qf[i].idx;
        vk->qf[i].num = in->qf[i].num;
        vk->qf[i].flags = (VkQueueFlagBits)in->qf[i].flags;
        vk->qf[i].video_caps = (VkVideoCodecOperationFlagBitsKHR)in->qf[i].video_caps;
    }
    int ret = av_hwdevice_ctx_init(ref);
    if (ret < 0) {
        set_err(err, errlen, "av_hwdevice_ctx_init(vulkan)", ret);
        av_buffer_unref(&ref);
        return NULL;
    }
    QsHwDevice *dev = av_mallocz(sizeof(*dev));
    dev->ref = ref;
    dev->hw = QS_HW_VULKAN;
    return dev;
}

QsHwDevice *qs_hw_device_create(int hw, char *err, int errlen) {
    enum AVHWDeviceType type;
    switch (hw) {
    case QS_HW_CUDA: type = AV_HWDEVICE_TYPE_CUDA; break;
    case QS_HW_VAAPI: type = AV_HWDEVICE_TYPE_VAAPI; break;
    case QS_HW_VULKAN: type = AV_HWDEVICE_TYPE_VULKAN; break;
    default:
        set_err(err, errlen, "no such hardware device type", 0);
        return NULL;
    }
    AVBufferRef *ref = NULL;
    int ret = av_hwdevice_ctx_create(&ref, type, NULL, NULL, 0);
    if (ret < 0) {
        set_err(err, errlen, "av_hwdevice_ctx_create", ret);
        return NULL;
    }
    QsHwDevice *dev = av_mallocz(sizeof(*dev));
    dev->ref = ref;
    dev->hw = hw;
    return dev;
}

void qs_hw_device_free(QsHwDevice *dev) {
    if (!dev)
        return;
    av_buffer_unref(&dev->ref);
    av_free(dev);
}

/* --- decoding ----------------------------------------------------------------------------- */

static enum AVPixelFormat hw_pix_fmt(int hw) {
    switch (hw) {
    case QS_HW_VULKAN: return AV_PIX_FMT_VULKAN;
    case QS_HW_CUDA: return AV_PIX_FMT_CUDA;
    case QS_HW_VAAPI: return AV_PIX_FMT_VAAPI;
    default: return AV_PIX_FMT_NONE;
    }
}

static enum AVPixelFormat get_format(AVCodecContext *ctx, const enum AVPixelFormat *fmts) {
    QsDec *d = ctx->opaque;
    if (d->hw_pix_fmt != AV_PIX_FMT_NONE) {
        for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
            if (*p == d->hw_pix_fmt) {
                d->active = d->hw;
                return *p;
            }
        }
    }
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
        const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(*p);
        if (desc && !(desc->flags & AV_PIX_FMT_FLAG_HWACCEL)) {
            d->active = QS_HW_SOFTWARE;
            return *p;
        }
    }
    return AV_PIX_FMT_NONE;
}

static const AVCodec *pick_codec(int codec, int hw) {
    switch (codec) {
    case QS_CODEC_H264: return avcodec_find_decoder(AV_CODEC_ID_H264);
    case QS_CODEC_HEVC: return avcodec_find_decoder(AV_CODEC_ID_HEVC);
    case QS_CODEC_AV1:
        if (hw != QS_HW_SOFTWARE)
            return avcodec_find_decoder_by_name("av1"); /* the hwaccel-only native decoder */
        {
            const AVCodec *c = avcodec_find_decoder_by_name("libdav1d");
            return c ? c : avcodec_find_decoder_by_name("libaom-av1");
        }
    default: return NULL;
    }
}

QsDec *qs_dec_open(int codec, QsHwDevice *dev, int threads, char *err, int errlen) {
    int hw = dev ? dev->hw : QS_HW_SOFTWARE;
    const AVCodec *c = pick_codec(codec, hw);
    if (!c) {
        set_err(err, errlen, "no decoder for this codec", 0);
        return NULL;
    }
    if (dev) {
        int ok = 0;
        for (int i = 0;; i++) {
            const AVCodecHWConfig *cfg = avcodec_get_hw_config(c, i);
            if (!cfg)
                break;
            if ((cfg->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) &&
                cfg->pix_fmt == hw_pix_fmt(hw)) {
                ok = 1;
                break;
            }
        }
        if (!ok) {
            set_err(err, errlen, "the decoder has no hwaccel for this device", 0);
            return NULL;
        }
    }
    QsDec *d = av_mallocz(sizeof(*d));
    d->hw = hw;
    d->active = hw;
    d->hw_pix_fmt = hw_pix_fmt(hw);
    d->ctx = avcodec_alloc_context3(c);
    d->pkt = av_packet_alloc();
    AVCodecContext *ctx = d->ctx;
    ctx->opaque = d;
    /* Output each frame as soon as it is decoded: no reordering delay. */
    ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    AVDictionary *opts = NULL;
    if (dev) {
        ctx->hw_device_ctx = av_buffer_ref(dev->ref);
        ctx->get_format = get_format;
        ctx->thread_count = 1;
        /* Frames the client holds (one being converted, one queued) beyond the DPB. */
        ctx->extra_hw_frames = 4;
    } else {
        /* Slice threads only: frame threads add a frame of delay per thread. */
        ctx->thread_type = FF_THREAD_SLICE;
        ctx->thread_count = threads;
        if (!strcmp(c->name, "libdav1d")) {
            av_dict_set(&opts, "max_frame_delay", "1", 0);
        } else if (!strcmp(c->name, "libaom-av1")) {
            ctx->thread_count = threads;
        }
    }
    int ret = avcodec_open2(ctx, c, &opts);
    av_dict_free(&opts);
    if (ret < 0) {
        set_err(err, errlen, "avcodec_open2", ret);
        qs_dec_close(d);
        return NULL;
    }
    return d;
}

int qs_dec_backend(const QsDec *d) { return d->active; }

const char *qs_dec_name(const QsDec *d) { return d->ctx->codec->name; }

int qs_dec_send(QsDec *d, const uint8_t *data, int size, int64_t pts) {
    AVPacket *pkt = d->pkt;
    av_packet_unref(pkt);
    /* Not refcounted: avcodec_send_packet copies it into a padded buffer. */
    pkt->data = (uint8_t *)data;
    pkt->size = size;
    pkt->pts = pts;
    pkt->dts = pts;
    int ret = avcodec_send_packet(d->ctx, pkt);
    pkt->data = NULL;
    pkt->size = 0;
    return ret;
}

static int describe(QsPicture *pic, enum AVPixelFormat fmt) {
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(fmt);
    if (!desc || (desc->flags & (AV_PIX_FMT_FLAG_RGB | AV_PIX_FMT_FLAG_HWACCEL)))
        return AVERROR(ENOSYS);
    int planes = av_pix_fmt_count_planes(fmt);
    if (desc->nb_components < 3 || (planes != 2 && planes != 3))
        return AVERROR(ENOSYS);
    pic->nb_planes = planes;
    pic->log2_chroma_w = desc->log2_chroma_w;
    pic->log2_chroma_h = desc->log2_chroma_h;
    pic->depth = desc->comp[0].depth;
    pic->bytes = desc->comp[0].depth > 8 ? 2 : 1;
    pic->shift = desc->comp[0].shift;
    if (desc->flags & AV_PIX_FMT_FLAG_BE)
        return AVERROR(ENOSYS);
    return 0;
}

int qs_dec_receive(QsDec *d, QsPicture *pic) {
    memset(pic, 0, sizeof(*pic));
    AVFrame *frame = av_frame_alloc();
    int ret = avcodec_receive_frame(d->ctx, frame);
    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
        av_frame_free(&frame);
        return 0;
    }
    if (ret < 0) {
        av_frame_free(&frame);
        return ret;
    }
    pic->pts = frame->pts != AV_NOPTS_VALUE ? frame->pts : frame->best_effort_timestamp;
    pic->color_range = frame->color_range;
    pic->colorspace = frame->colorspace;
    pic->color_trc = frame->color_trc;
    pic->color_primaries = frame->color_primaries;
    pic->width = frame->width;
    pic->height = frame->height;
    if (frame->format == AV_PIX_FMT_VULKAN) {
        AVHWFramesContext *fc = (AVHWFramesContext *)frame->hw_frames_ctx->data;
        ret = describe(pic, fc->sw_format);
        if (ret < 0) {
            av_frame_free(&frame);
            return ret;
        }
        pic->kind = QS_PIC_VULKAN;
        pic->frame = frame;
        return 1;
    }
    if (frame->hw_frames_ctx) {
        /* NVDEC or VAAPI: copy to memory (not zero-copy). */
        int64_t t0 = now_us();
        AVFrame *sw = av_frame_alloc();
        ret = av_hwframe_transfer_data(sw, frame, 0);
        if (ret < 0) {
            av_frame_free(&sw);
            av_frame_free(&frame);
            return ret;
        }
        av_frame_copy_props(sw, frame);
        av_frame_free(&frame);
        frame = sw;
        pic->transfer_us = now_us() - t0;
    }
    ret = describe(pic, frame->format);
    if (ret < 0) {
        av_frame_free(&frame);
        return ret;
    }
    pic->kind = QS_PIC_CPU;
    pic->frame = frame;
    for (int i = 0; i < pic->nb_planes; i++) {
        pic->data[i] = frame->data[i];
        pic->linesize[i] = frame->linesize[i];
    }
    return 1;
}

void qs_dec_flush(QsDec *d) { avcodec_flush_buffers(d->ctx); }

void qs_dec_close(QsDec *d) {
    if (!d)
        return;
    avcodec_free_context(&d->ctx);
    av_packet_free(&d->pkt);
    av_free(d);
}

void qs_pic_free(QsPicture *pic) {
    if (pic && pic->frame) {
        AVFrame *f = pic->frame;
        av_frame_free(&f);
        pic->frame = NULL;
    }
}

int qs_pic_vk_lock(QsPicture *pic, QsVkImage *out) {
    AVFrame *frame = pic->frame;
    if (!frame || frame->format != AV_PIX_FMT_VULKAN)
        return AVERROR(EINVAL);
    AVHWFramesContext *fc = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    AVVulkanFramesContext *vfc = fc->hwctx;
    AVVkFrame *f = (AVVkFrame *)frame->data[0];
    vfc->lock_frame(fc, f);
    memset(out, 0, sizeof(*out));
    out->image = (uint64_t)(uintptr_t)f->img[0];
    out->format = vfc->format[0];
    out->layout = f->layout[0];
    out->access = f->access[0];
    out->semaphore = (uint64_t)(uintptr_t)f->sem[0];
    out->sem_value = f->sem_value[0];
    out->queue_family = f->queue_family[0];
    out->img_flags = vfc->img_flags;
    out->usage = vfc->usage;
    for (int i = 0; i < AV_NUM_DATA_POINTERS && f->img[i]; i++)
        out->nb_images = i + 1;
    out->width = fc->width;
    out->height = fc->height;
    return 0;
}

void qs_pic_vk_unlock(QsPicture *pic, const QsVkImage *in) {
    AVFrame *frame = pic->frame;
    AVHWFramesContext *fc = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    AVVulkanFramesContext *vfc = fc->hwctx;
    AVVkFrame *f = (AVVkFrame *)frame->data[0];
    f->layout[0] = in->layout;
    f->access[0] = in->access;
    f->sem_value[0] = in->sem_value;
    f->queue_family[0] = in->queue_family;
    vfc->unlock_frame(fc, f);
}

/* --- demuxing ----------------------------------------------------------------------------- */

QsDemux *qs_demux_open(const char *path, QsStreamInfo *info, char *err, int errlen) {
    AVFormatContext *fmt = NULL;
    int ret = avformat_open_input(&fmt, path, NULL, NULL);
    if (ret < 0) {
        set_err(err, errlen, "avformat_open_input", ret);
        return NULL;
    }
    ret = avformat_find_stream_info(fmt, NULL);
    if (ret < 0) {
        set_err(err, errlen, "avformat_find_stream_info", ret);
        avformat_close_input(&fmt);
        return NULL;
    }
    int s = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (s < 0) {
        set_err(err, errlen, "no video stream", s);
        avformat_close_input(&fmt);
        return NULL;
    }
    AVCodecParameters *par = fmt->streams[s]->codecpar;
    switch (par->codec_id) {
    case AV_CODEC_ID_H264: info->codec = QS_CODEC_H264; break;
    case AV_CODEC_ID_HEVC: info->codec = QS_CODEC_HEVC; break;
    case AV_CODEC_ID_AV1: info->codec = QS_CODEC_AV1; break;
    default:
        set_err(err, errlen, "the video is not H.264, HEVC or AV1", 0);
        avformat_close_input(&fmt);
        return NULL;
    }
    info->width = par->width;
    info->height = par->height;
    AVRational r = fmt->streams[s]->avg_frame_rate;
    if (r.num <= 0 || r.den <= 0)
        r = fmt->streams[s]->r_frame_rate;
    info->fps_num = r.num;
    info->fps_den = r.den;
    QsDemux *d = av_mallocz(sizeof(*d));
    d->fmt = fmt;
    d->pkt = av_packet_alloc();
    d->stream = s;
    return d;
}

int qs_demux_read(QsDemux *d, QsPacket *out) {
    for (;;) {
        av_packet_unref(d->pkt);
        int ret = av_read_frame(d->fmt, d->pkt);
        if (ret == AVERROR_EOF)
            return 0;
        if (ret < 0)
            return ret;
        if (d->pkt->stream_index != d->stream)
            continue;
        out->data = d->pkt->data;
        out->size = d->pkt->size;
        out->key = (d->pkt->flags & AV_PKT_FLAG_KEY) != 0;
        return 1;
    }
}

void qs_demux_close(QsDemux *d) {
    if (!d)
        return;
    av_packet_free(&d->pkt);
    avformat_close_input(&d->fmt);
    av_free(d);
}
