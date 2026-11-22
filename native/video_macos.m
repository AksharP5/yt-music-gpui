/* macOS video: the macOS counterpart of video.c, with the same interface.
   A view sits below GPUI's view in the window's content view, like the Wayland
   subsurface, so the app's menus and dialogs draw over it. A worker thread renders
   mpv into IOSurfaces with an offscreen OpenGL context and shows each one as the
   view's layer contents, so AppKit never calls back into the renderer.
   Built without ARC: the struct holds its views with manual retains. */
#define GL_SILENCE_DEPRECATION
#import <Cocoa/Cocoa.h>
#import <QuartzCore/QuartzCore.h>
#include <IOSurface/IOSurface.h>
#include <OpenGL/OpenGL.h>
#include <OpenGL/CGLIOSurface.h>
#include <OpenGL/gl3.h>
#include <mpv/client.h>
#include <mpv/render_gl.h>
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>

/* The window server can still be compositing the last surface or two shown. */
#define BUFFERS 3

/* Clicks reach GPUI's view above, never the video. */
@interface YTMVideoView : NSView
@end
@implementation YTMVideoView
- (NSView *)hitTest:(NSPoint)point {
    (void)point;
    return nil;
}
@end

struct video {
    NSView *app_view;
    YTMVideoView *view;
    CALayer *layer;
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    pthread_t worker;
    int started, stop, status, wake, geometry_dirty, notify_fd;
    int x, y, width, height, pixel_width, pixel_height;
    mpv_handle *core;
};

/* Main thread only: the view's place in the window. */
static void place_view(struct video *v) {
    NSRect rect = [v->app_view convertRectFromBacking:NSMakeRect(v->x, v->y, v->width, v->height)];
    if (![v->app_view isFlipped])
        rect.origin.y = NSHeight([v->app_view bounds]) - NSMaxY(rect);
    NSRect frame = [v->app_view convertRect:rect toView:[v->view superview]];
    [CATransaction begin];
    [CATransaction setDisableActions:YES];
    [v->view setFrame:frame];
    [CATransaction commit];
}

struct video *video_create(void *app_view) {
    if (![NSThread isMainThread]) return NULL;
    NSView *parent = (NSView *)app_view;
    NSView *container = [parent superview];
    if (!container) return NULL;
    struct video *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    pthread_mutex_init(&v->mutex, NULL);
    pthread_cond_init(&v->condition, NULL);
    v->app_view = [parent retain];
    v->layer = [[CALayer alloc] init];
    v->layer.contentsGravity = kCAGravityResize;
    v->view = [[YTMVideoView alloc] initWithFrame:NSZeroRect];
    [v->view setLayer:v->layer];
    [v->view setWantsLayer:YES];
    [container addSubview:v->view positioned:NSWindowBelow relativeTo:parent];
    return v;
}

/* Main thread: `x`, `y`, `width` and `height` are backing pixels from the top
   left of GPUI's view, and the pixel sizes are the frame's. */
int video_resize(struct video *v, int x, int y, int width, int height, int pixel_width, int pixel_height) {
    if (width < 1 || height < 1 || pixel_width < 1 || pixel_height < 1) return 0;
    if (![NSThread isMainThread]) return 0;
    pthread_mutex_lock(&v->mutex);
    int changed = v->x != x || v->y != y || v->width != width || v->height != height;
    int resized = v->pixel_width != pixel_width || v->pixel_height != pixel_height;
    v->x = x; v->y = y; v->width = width; v->height = height;
    v->pixel_width = pixel_width; v->pixel_height = pixel_height;
    if (changed || resized) {
        v->geometry_dirty = 1;
        pthread_cond_signal(&v->condition);
    }
    pthread_mutex_unlock(&v->mutex);
    /* The window can move GPUI's view (fullscreen, a toolbar), so place it every time. */
    place_view(v);
    return 1;
}

/* Serialized by the Rust owner; safe on the actor or window thread. */
void video_stop(struct video *v) {
    if (!v->started) return;
    pthread_mutex_lock(&v->mutex);
    v->stop = 1;
    pthread_cond_signal(&v->condition);
    pthread_mutex_unlock(&v->mutex);
    pthread_join(v->worker, NULL);
    v->started = 0;
    v->core = NULL;
}

/* Cleanup runs in the background; views leave the window on the main thread. */
void video_destroy(struct video *v) {
    if (!v) return;
    video_stop(v);
    NSView *view = v->view, *app_view = v->app_view;
    CALayer *layer = v->layer;
    void (^remove)(void) = ^{
        [view removeFromSuperview];
        [view release];
        [layer release];
        [app_view release];
    };
    if ([NSThread isMainThread]) remove();
    else dispatch_async(dispatch_get_main_queue(), remove);
    pthread_cond_destroy(&v->condition);
    pthread_mutex_destroy(&v->mutex);
    free(v);
}

static void wake_worker(void *data) {
    struct video *v = data;
    pthread_mutex_lock(&v->mutex);
    v->wake = 1;
    pthread_cond_signal(&v->condition);
    pthread_mutex_unlock(&v->mutex);
}

static void *get_proc_address(void *context, const char *name) {
    (void)context;
    static void *opengl;
    if (!opengl) opengl = dlopen("/System/Library/Frameworks/OpenGL.framework/OpenGL", RTLD_LAZY | RTLD_LOCAL);
    return opengl ? dlsym(opengl, name) : NULL;
}

/* The actor owns notify_fd until video_stop joins this worker. Wake only on
   status changes, never once per frame. */
static void set_status(struct video *v, int status) {
    pthread_mutex_lock(&v->mutex);
    int changed = v->status != status;
    v->status = status;
    pthread_mutex_unlock(&v->mutex);
    if (changed) {
        uint64_t one = 1;
        while (write(v->notify_fd, &one, sizeof(one)) < 0 && errno == EINTR) {}
    }
}

struct buffer {
    IOSurfaceRef surface;
    GLuint texture, framebuffer;
};

static void release_buffers(struct buffer *buffers) {
    for (int i = 0; i < BUFFERS; i++) {
        if (buffers[i].framebuffer) glDeleteFramebuffers(1, &buffers[i].framebuffer);
        if (buffers[i].texture) glDeleteTextures(1, &buffers[i].texture);
        if (buffers[i].surface) CFRelease(buffers[i].surface);
        buffers[i] = (struct buffer){0};
    }
}

static int create_buffers(CGLContextObj gl, struct buffer *buffers, int width, int height) {
    NSDictionary *properties = @{
        (id)kIOSurfaceWidth: @(width),
        (id)kIOSurfaceHeight: @(height),
        (id)kIOSurfaceBytesPerElement: @4,
        (id)kIOSurfacePixelFormat: @((uint32_t)'BGRA'),
    };
    for (int i = 0; i < BUFFERS; i++) {
        struct buffer *b = &buffers[i];
        b->surface = IOSurfaceCreate((CFDictionaryRef)properties);
        if (!b->surface) return 0;
        glGenTextures(1, &b->texture);
        glBindTexture(GL_TEXTURE_RECTANGLE, b->texture);
        if (CGLTexImageIOSurface2D(gl, GL_TEXTURE_RECTANGLE, GL_RGBA8, width, height,
                GL_BGRA, GL_UNSIGNED_INT_8_8_8_8_REV, b->surface, 0) != kCGLNoError) return 0;
        glGenFramebuffers(1, &b->framebuffer);
        glBindFramebuffer(GL_FRAMEBUFFER, b->framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, b->texture, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return 0;
    }
    glBindTexture(GL_TEXTURE_RECTANGLE, 0);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    return 1;
}

static void *render_worker(void *data) {
    struct video *v = data;
    CGLContextObj gl = NULL;
    mpv_render_context *render = NULL;
    struct buffer buffers[BUFFERS] = {{0}};
    int status = -1, next = 0, buffer_width = 0, buffer_height = 0;
    CGLPixelFormatAttribute attributes[] = {
        kCGLPFAOpenGLProfile, (CGLPixelFormatAttribute)kCGLOGLPVersion_3_2_Core,
        kCGLPFAAccelerated, kCGLPFAAllowOfflineRenderers, 0
    };
    CGLPixelFormatObj format = NULL;
    GLint formats = 0;
    if (CGLChoosePixelFormat(attributes, &format, &formats) != kCGLNoError || !format) goto done;
    CGLError created = CGLCreateContext(format, NULL, &gl);
    CGLReleasePixelFormat(format);
    if (created != kCGLNoError || !gl || CGLSetCurrentContext(gl) != kCGLNoError) goto done;
    mpv_opengl_init_params init_gl = {get_proc_address, NULL};
    int advanced = 1;
    mpv_render_param init[] = {
        {MPV_RENDER_PARAM_API_TYPE, MPV_RENDER_API_TYPE_OPENGL},
        {MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, &init_gl},
        {MPV_RENDER_PARAM_ADVANCED_CONTROL, &advanced},
        {MPV_RENDER_PARAM_INVALID, NULL}
    };
    if (mpv_render_context_create(&render, v->core, init) < 0) goto done;
    mpv_render_context_set_update_callback(render, wake_worker, v);
    set_status(v, 1);
    for (;;) {
        pthread_mutex_lock(&v->mutex);
        while (!v->wake && !v->geometry_dirty && !v->stop)
            pthread_cond_wait(&v->condition, &v->mutex);
        int stop = v->stop, dirty = v->geometry_dirty;
        int width = v->pixel_width, height = v->pixel_height;
        v->wake = 0;
        v->geometry_dirty = 0;
        pthread_mutex_unlock(&v->mutex);
        if (stop) { status = 3; break; }
        if (width < 1 || height < 1) continue;
        if (width != buffer_width || height != buffer_height) {
            release_buffers(buffers);
            buffer_width = buffer_height = 0;
            int ok;
            @autoreleasepool { ok = create_buffers(gl, buffers, width, height); }
            if (!ok) { status = -3; break; }
            buffer_width = width;
            buffer_height = height;
        }
        uint64_t flags = mpv_render_context_update(render);
        if (!dirty && !(flags & MPV_RENDER_UPDATE_FRAME)) continue;
        /* Skip surfaces the window server is still reading. */
        for (int i = 0; i < BUFFERS && IOSurfaceIsInUse(buffers[next].surface); i++)
            next = (next + 1) % BUFFERS;
        struct buffer *b = &buffers[next];
        next = (next + 1) % BUFFERS;
        mpv_opengl_fbo fbo = {b->framebuffer, width, height, 0};
        mpv_render_param params[] = {
            {MPV_RENDER_PARAM_OPENGL_FBO, &fbo},
            {MPV_RENDER_PARAM_INVALID, NULL}
        };
        if (mpv_render_context_render(render, params) < 0) { status = -2; break; }
        glFlush();
        @autoreleasepool {
            [CATransaction begin];
            [CATransaction setDisableActions:YES];
            v->layer.contents = (id)b->surface;
            [CATransaction commit];
        }
        mpv_render_context_report_swap(render);
        set_status(v, 2);
    }
done:
    if (render) {
        mpv_render_context_set_update_callback(render, NULL, NULL);
        mpv_render_context_free(render);
    }
    if (gl) {
        release_buffers(buffers);
        CGLSetCurrentContext(NULL);
        CGLReleaseContext(gl);
    }
    set_status(v, status);
    return NULL;
}

int video_start(struct video *v, mpv_handle *core, int notify_fd) {
    if (v->started) return 0;
    v->core = core;
    v->notify_fd = notify_fd;
    v->wake = 1;
    v->geometry_dirty = 1;
    v->status = 0;
    v->stop = 0;
    v->started = pthread_create(&v->worker, NULL, render_worker, v) == 0;
    if (!v->started) { v->status = -1; return 0; }
    return 1;
}

/* Zero before start, one before the first frame, two after it, three after stop. */
int video_status(struct video *v) {
    pthread_mutex_lock(&v->mutex);
    int status = v->status;
    pthread_mutex_unlock(&v->mutex);
    return status;
}
