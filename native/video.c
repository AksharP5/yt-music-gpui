#define WL_EGL_PLATFORM 1
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/shape.h>
#include <wayland-client.h>
#include <wayland-egl.h>
#include <mpv/client.h>
#include <mpv/render_gl.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include "viewporter-client-protocol.h"

int video_initialize(void) {
    return XInitThreads() != 0;
}

struct video {
    Display *x_display;
    Window x_parent, x_child;
    Colormap x_colormap;
    struct wl_display *display;
    struct wl_surface *parent;
    struct wl_event_queue *queue;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_subcompositor *subcompositor;
    struct wp_viewporter *viewporter;
    struct wp_viewport *viewport;
    struct wl_surface *surface;
    struct wl_subsurface *subsurface;
    struct wl_egl_window *window;
    EGLDisplay egl_display;
    EGLContext context;
    EGLSurface egl_surface;
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    pthread_t worker;
    int started, stop, status, wake, geometry_dirty, x11;
    int x, y, width, height, pixel_width, pixel_height;
    mpv_handle *core;
};

static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    struct video *v = data;
    if (strcmp(interface, "wl_compositor") == 0)
        v->compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                        version < 4 ? version : 4);
    if (strcmp(interface, "wl_subcompositor") == 0)
        v->subcompositor = wl_registry_bind(registry, name, &wl_subcompositor_interface, 1);
    if (strcmp(interface, "wp_viewporter") == 0)
        v->viewporter = wl_registry_bind(registry, name, &wp_viewporter_interface, 1);
}
static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {global, global_remove};
static void surface_enter(void *data, struct wl_surface *surface, struct wl_output *output) {
    (void)data; (void)surface; (void)output;
}
static void surface_leave(void *data, struct wl_surface *surface, struct wl_output *output) {
    (void)data; (void)surface; (void)output;
}
static const struct wl_surface_listener surface_listener = {
    .enter = surface_enter, .leave = surface_leave,
};

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

/* Cleanup runs in the background; Rust fences it before the window/display dies. */
void video_destroy(struct video *v) {
    if (!v) return;
    video_stop(v);
    if (v->egl_display != EGL_NO_DISPLAY) {
        if (v->egl_surface != EGL_NO_SURFACE) eglDestroySurface(v->egl_display, v->egl_surface);
        if (v->context != EGL_NO_CONTEXT) eglDestroyContext(v->egl_display, v->context);
        /* Only the X11 display belongs to us; Wayland's may be shared with GPUI. */
        if (v->x_display) eglTerminate(v->egl_display);
    }
    if (v->x_child) XDestroyWindow(v->x_display, v->x_child);
    if (v->x_colormap) XFreeColormap(v->x_display, v->x_colormap);
    if (v->x_display) XCloseDisplay(v->x_display);
    if (v->window) wl_egl_window_destroy(v->window);
    if (v->viewport) wp_viewport_destroy(v->viewport);
    if (v->subsurface) wl_subsurface_destroy(v->subsurface);
    if (v->surface) wl_surface_destroy(v->surface);
    if (v->viewporter) wp_viewporter_destroy(v->viewporter);
    if (v->subcompositor) wl_subcompositor_destroy(v->subcompositor);
    if (v->compositor) wl_compositor_destroy(v->compositor);
    if (v->registry) wl_registry_destroy(v->registry);
    if (v->display) wl_display_flush(v->display);
    if (v->queue) wl_event_queue_destroy(v->queue);
    pthread_cond_destroy(&v->condition);
    pthread_mutex_destroy(&v->mutex);
    free(v);
}

struct video *video_create(void *display, void *parent) {
    struct video *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    pthread_mutex_init(&v->mutex, NULL);
    pthread_cond_init(&v->condition, NULL);
    v->display = display;
    v->parent = parent;
    v->egl_display = EGL_NO_DISPLAY;
    v->context = EGL_NO_CONTEXT;
    v->egl_surface = EGL_NO_SURFACE;
    v->queue = wl_display_create_queue(v->display);
    if (!v->queue) goto fail;
    struct wl_display *wrapper = wl_proxy_create_wrapper(v->display);
    if (!wrapper) goto fail;
    wl_proxy_set_queue((struct wl_proxy *)wrapper, v->queue);
    v->registry = wl_display_get_registry(wrapper);
    wl_proxy_wrapper_destroy(wrapper);
    if (!v->registry) goto fail;
    wl_registry_add_listener(v->registry, &registry_listener, v);
    if (wl_display_roundtrip_queue(v->display, v->queue) < 0) goto fail;
    if (!v->compositor || !v->subcompositor || !v->viewporter) goto fail;
    v->surface = wl_compositor_create_surface(v->compositor);
    if (!v->surface) goto fail;
    wl_surface_add_listener(v->surface, &surface_listener, v);
    v->subsurface = wl_subcompositor_get_subsurface(v->subcompositor, v->surface, v->parent);
    if (!v->subsurface) goto fail;
    wl_subsurface_set_desync(v->subsurface);
    wl_subsurface_place_below(v->subsurface, v->parent);
    v->viewport = wp_viewporter_get_viewport(v->viewporter, v->surface);
    if (!v->viewport) goto fail;
    struct wl_region *input = wl_compositor_create_region(v->compositor);
    if (!input) goto fail;
    wl_surface_set_input_region(v->surface, input);
    wl_region_destroy(input);
    wl_surface_set_buffer_scale(v->surface, 1);
    v->window = wl_egl_window_create(v->surface, 1, 1);
    if (!v->window) goto fail;

    return v;
fail:
    video_destroy(v);
    return NULL;
}

struct video *video_create_x11(unsigned long parent) {
    struct video *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    pthread_mutex_init(&v->mutex, NULL);
    pthread_cond_init(&v->condition, NULL);
    v->x_parent = parent;
    v->egl_display = EGL_NO_DISPLAY;
    v->context = EGL_NO_CONTEXT;
    v->egl_surface = EGL_NO_SURFACE;
    v->x11 = 1;
    return v;
}

int video_resize(struct video *v, int x, int y, int width, int height, int pixel_width, int pixel_height) {
    if (width < 1 || height < 1 || pixel_width < 1 || pixel_height < 1) return 0;
    pthread_mutex_lock(&v->mutex);
    if (v->x == x && v->y == y && v->width == width && v->height == height &&
        v->pixel_width == pixel_width && v->pixel_height == pixel_height) {
        pthread_mutex_unlock(&v->mutex);
        return 1;
    }
    v->x = x; v->y = y; v->width = width; v->height = height;
    v->pixel_width = pixel_width; v->pixel_height = pixel_height;
    if (!v->x11) {
        wl_subsurface_set_position(v->subsurface, x, y);
        wp_viewport_set_destination(v->viewport, width, height);
    }
    v->geometry_dirty = 1;
    pthread_cond_signal(&v->condition);
    pthread_mutex_unlock(&v->mutex);
    return 1;
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
    return (void *)eglGetProcAddress(name);
}
static int initialize_egl(struct video *v) {
    XWindowAttributes parent_attributes;
    if (v->x11) {
        v->x_display = XOpenDisplay(NULL);
        if (!v->x_display) return 0;
        if (!XGetWindowAttributes(v->x_display, v->x_parent, &parent_attributes)) return 0;
        int shape_event, shape_error;
        if (!XShapeQueryExtension(v->x_display, &shape_event, &shape_error)) return 0;
        v->egl_display = eglGetPlatformDisplay(EGL_PLATFORM_X11_KHR, v->x_display, NULL);
    } else {
        v->egl_display = eglGetPlatformDisplay(EGL_PLATFORM_WAYLAND_KHR, v->display, NULL);
    }
    EGLint major, minor;
    if (v->egl_display == EGL_NO_DISPLAY || !eglInitialize(v->egl_display, &major, &minor)) return 0;
    if (!eglBindAPI(EGL_OPENGL_ES_API)) return 0;
    const EGLint attributes[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, v->x11 ? 0 : 8, EGL_NONE
    };
    EGLConfig config;
    EGLint count, visual_id;
    if (!eglChooseConfig(v->egl_display, attributes, &config, 1, &count) || count < 1) return 0;
    if (v->x11) {
        if (!eglGetConfigAttrib(v->egl_display, config, EGL_NATIVE_VISUAL_ID, &visual_id)) return 0;
        XVisualInfo wanted = { .visualid = visual_id, .screen = XScreenNumberOfScreen(parent_attributes.screen) };
        int matches;
        XVisualInfo *visual = XGetVisualInfo(v->x_display, VisualIDMask | VisualScreenMask, &wanted, &matches);
        if (!visual || matches < 1) { if (visual) XFree(visual); return 0; }
        v->x_colormap = XCreateColormap(v->x_display, parent_attributes.root, visual->visual, AllocNone);
        if (!v->x_colormap) { XFree(visual); return 0; }
        XSetWindowAttributes child_attributes = { .colormap=v->x_colormap, .border_pixel=0, .background_pixel=0 };
        v->x_child = XCreateWindow(v->x_display, v->x_parent, 0, 0, 1, 1, 0, visual->depth,
            InputOutput, visual->visual, CWColormap | CWBorderPixel | CWBackPixel, &child_attributes);
        XFree(visual);
        if (!v->x_child) return 0;
        XShapeCombineRectangles(v->x_display, v->x_child, ShapeInput, 0, 0, NULL, 0, ShapeSet, Unsorted);
        int n = -1, order;
        XRectangle *input = XShapeGetRectangles(v->x_display, v->x_child, ShapeInput, &n, &order);
        if (input) XFree(input);
        if (n != 0) return 0;
    }
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    v->context = eglCreateContext(v->egl_display, config, EGL_NO_CONTEXT, context_attributes);
    v->egl_surface = eglCreateWindowSurface(v->egl_display, config,
        v->x11 ? (EGLNativeWindowType)v->x_child : (EGLNativeWindowType)v->window, NULL);
    return v->context != EGL_NO_CONTEXT && v->egl_surface != EGL_NO_SURFACE;
}

static void *render_worker(void *data) {
    struct video *v = data;
    mpv_render_context *render = NULL;
    int status = -1, mapped = 0;
    if (!initialize_egl(v)) goto done;
    eglBindAPI(EGL_OPENGL_ES_API);
    if (!eglMakeCurrent(v->egl_display, v->egl_surface, v->egl_surface, v->context)) goto done;
    eglSwapInterval(v->egl_display, 0);
    mpv_opengl_init_params gl = {get_proc_address, NULL};
    int advanced = 1;
    mpv_render_param init[] = {
        {MPV_RENDER_PARAM_API_TYPE, MPV_RENDER_API_TYPE_OPENGL},
        {MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, &gl},
        {v->x_display ? MPV_RENDER_PARAM_X11_DISPLAY : MPV_RENDER_PARAM_WL_DISPLAY,
         v->x_display ? (void *)v->x_display : (void *)v->display},
        {MPV_RENDER_PARAM_ADVANCED_CONTROL, &advanced},
        {MPV_RENDER_PARAM_INVALID, NULL}
    };
    int error = mpv_render_context_create(&render, v->core, init);
    if (error < 0) {
        goto done;
    }
    mpv_render_context_set_update_callback(render, wake_worker, v);
    pthread_mutex_lock(&v->mutex);
    v->status = 1;
    pthread_cond_signal(&v->condition);
    pthread_mutex_unlock(&v->mutex);
    for (;;) {
        pthread_mutex_lock(&v->mutex);
        while (!v->wake && !v->geometry_dirty && !v->stop)
            pthread_cond_wait(&v->condition, &v->mutex);
        int stop = v->stop, dirty = v->geometry_dirty;
        int width = v->pixel_width, height = v->pixel_height;
        int x = v->x, y = v->y;
        v->wake = 0;
        v->geometry_dirty = 0;
        pthread_mutex_unlock(&v->mutex);
        if (stop) { status = 3; break; }
        if (width < 1 || height < 1) continue;
        if (dirty) {
            if (v->x_display) {
                XMoveResizeWindow(v->x_display, v->x_child, x, y, width, height);
                if (!mapped) { XMapWindow(v->x_display, v->x_child); mapped = 1; }
                XFlush(v->x_display);
            } else {
                wl_egl_window_resize(v->window, width, height, 0, 0);
            }
        }
        uint64_t flags = mpv_render_context_update(render);
        if (dirty || (flags & MPV_RENDER_UPDATE_FRAME)) {
            mpv_opengl_fbo fbo = {0, width, height, 0};
            int flip = 1;
            mpv_render_param params[] = {
                {MPV_RENDER_PARAM_OPENGL_FBO, &fbo},
                {MPV_RENDER_PARAM_FLIP_Y, &flip},
                {MPV_RENDER_PARAM_INVALID, NULL}
            };
            error = mpv_render_context_render(render, params);
            if (error < 0) { status = -2; break; }
            if (!eglSwapBuffers(v->egl_display, v->egl_surface)) { status = -3; break; }
            mpv_render_context_report_swap(render);
            pthread_mutex_lock(&v->mutex);
            v->status = 2;
            pthread_mutex_unlock(&v->mutex);
            if (v->x_display) {
                XFlush(v->x_display);
            } else {
                if (wl_display_dispatch_queue_pending(v->display, v->queue) < 0) {
                    status = -4;
                    break;
                }
                wl_display_flush(v->display);
            }
        }
    }
done:
    if (render) {
        mpv_render_context_set_update_callback(render, NULL, NULL);
        mpv_render_context_free(render);
    }
    if (v->egl_display != EGL_NO_DISPLAY)
        eglMakeCurrent(v->egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    pthread_mutex_lock(&v->mutex);
    v->status = status;
    pthread_cond_signal(&v->condition);
    pthread_mutex_unlock(&v->mutex);
    return NULL;
}

int video_start(struct video *v, mpv_handle *core) {
    if (v->started) return 0;
    v->core = core;
    v->wake = 1;
    v->geometry_dirty = 1;
    v->status = 0;
    v->stop = 0;
    v->started = pthread_create(&v->worker, NULL, render_worker, v) == 0;
    if (!v->started) { v->status = -1; return 0; }
    /* Renderer initialization can take hundreds of milliseconds. The actor/UI
       observes video_status instead of waiting for EGL/mpv initialization. */
    return 1;
}

/* Zero before start, one before the first swap, two after it, three after stop. */
int video_status(struct video *v) {
    pthread_mutex_lock(&v->mutex);
    int status = v->status;
    pthread_mutex_unlock(&v->mutex);
    return status;
}
