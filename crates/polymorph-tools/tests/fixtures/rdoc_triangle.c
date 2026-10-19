// Minimal presenting OpenGL sample for RenderDoc capture proofs (chunk 112).
// Win32 + WGL 3.3 CORE context + shader triangle. No external deps beyond
// opengl32/gdi32 (in-box on Windows). Renders FRAMES frames, prints
// "frames=N" to stdout, exits 0 — no user input (automation-safe).
//
// RenderDoc REFUSES legacy contexts by design (FAQ: no CreateContextAttribs
// => assume legacy => refuse) — hence the dummy-context Attribs dance and
// real shaders below. Immediate-mode would render fine but stay invisible.
//
// In-app capture (renderdoc_app.h ships in the RenderDoc install): when
// launched under `renderdoccmd capture` the renderdoc module is present —
// frame CAP_FRAME is captured programmatically via TriggerCapture.
// Standalone (no RenderDoc) the API is absent and the sample just renders.
//
// Build (ucrt64 gcc FIRST on PATH for its cc1 — bare absolute path fails
// silent exit 1; host roster in docs/renderdoc.md):
//   export PATH=/c/msys64/ucrt64/bin:$PATH
//   gcc -O2 -I"C:/Program Files/RenderDoc" -o triangle.exe rdoc_triangle.c -lopengl32 -lgdi32
#include <stdio.h>
#include <windows.h>
#include <GL/gl.h>
#include "renderdoc_app.h"

// GL 3.x entry points we need (loaded manually — no loader dep).
typedef ptrdiff_t GLsizeiptr;
typedef char GLchar;
#define GL_ARRAY_BUFFER 0x8892
#define GL_STATIC_DRAW 0x88E4
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_VERTEX_SHADER 0x8B31
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
typedef unsigned (*PFNGLCREATESHADERPROC)(unsigned);
typedef void (*PFNGLSHADERSOURCEPROC)(unsigned, int, const GLchar *const *, const int *);
typedef void (*PFNGLCOMPILESHADERPROC)(unsigned);
typedef unsigned (*PFNGLCREATEPROGRAMPROC)(void);
typedef void (*PFNGLATTACHSHADERPROC)(unsigned, unsigned);
typedef void (*PFNGLLINKPROGRAMPROC)(unsigned);
typedef void (*PFNGLUSEPROGRAMPROC)(unsigned);
typedef void (*PFNGLGENBUFFERSPROC)(int, unsigned *);
typedef void (*PFNGLBINDBUFFERPROC)(unsigned, unsigned);
typedef void (*PFNGLBUFFERDATAPROC)(unsigned, GLsizeiptr, const void *, unsigned);
typedef void (*PFNGLGENVERTEXARRAYSPROC)(int, unsigned *);
typedef void (*PFNGLBINDVERTEXARRAYPROC)(unsigned);
typedef void (*PFNGLENABLEVERTEXATTRIBARRAYPROC)(unsigned);
typedef void (*PFNGLVERTEXATTRIBPOINTERPROC)(unsigned, int, unsigned, unsigned char, int, const void *);
typedef HGLRC(WINAPI *PFNWGLCREATECONTEXTATTRIBSARBPROC)(HDC, HGLRC, const int *);
#define WGL_CONTEXT_MAJOR_VERSION_ARB 0x2091
#define WGL_CONTEXT_MINOR_VERSION_ARB 0x2092
#define WGL_CONTEXT_PROFILE_MASK_ARB 0x9126
#define WGL_CONTEXT_CORE_PROFILE_BIT_ARB 0x00000001

#define FRAMES 120
#define CAP_FRAME 5

static RENDERDOC_API_1_6_0 *rdoc = NULL;

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProc(h, m, w, l);
}

static HWND make_window(HINSTANCE inst) {
    WNDCLASSA wc = {0};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = inst;
    wc.lpszClassName = "rdoc_triangle";
    if (!RegisterClassA(&wc)) {
        return NULL;
    }
    return CreateWindowA(wc.lpszClassName, "rdoc_triangle", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                         100, 100, 640, 480, NULL, NULL, inst, NULL);
}

static int setup_pixel_format(HDC dc) {
    PIXELFORMATDESCRIPTOR pfd = {0};
    pfd.nSize = sizeof(pfd);
    pfd.nVersion = 1;
    pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    pfd.iPixelType = PFD_TYPE_RGBA;
    pfd.cColorBits = 24;
    int pf = ChoosePixelFormat(dc, &pfd);
    if (!pf || !SetPixelFormat(dc, pf, &pfd)) {
        return 0;
    }
    return 1;
}

static const char *vs_src =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "layout(location = 1) in vec3 col;\n"
    "out vec3 vcol;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); vcol = col; }\n";
static const char *fs_src =
    "#version 330 core\n"
    "in vec3 vcol;\n"
    "out vec4 outcol;\n"
    "void main() { outcol = vec4(vcol, 1.0); }\n";

static unsigned make_shader(PFNGLCREATESHADERPROC glCreateShader,
                            PFNGLSHADERSOURCEPROC glShaderSource,
                            PFNGLCOMPILESHADERPROC glCompileShader, unsigned kind,
                            const char *src) {
    unsigned s = glCreateShader(kind);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    return s;
}

int main(void) {
    HINSTANCE inst = GetModuleHandle(NULL);
    HWND hwnd = make_window(inst);
    if (!hwnd) {
        printf("create-window failed\n");
        return 1;
    }
    // Dummy legacy context: exists only to load wglCreateContextAttribsARB.
    HDC dummy_dc = GetDC(hwnd);
    if (!setup_pixel_format(dummy_dc)) {
        printf("pixel-format failed\n");
        return 1;
    }
    HGLRC dummy = wglCreateContext(dummy_dc);
    if (!dummy || !wglMakeCurrent(dummy_dc, dummy)) {
        printf("dummy-context failed\n");
        return 1;
    }
    PFNWGLCREATECONTEXTATTRIBSARBPROC wglCreateContextAttribsARB =
        (PFNWGLCREATECONTEXTATTRIBSARBPROC)wglGetProcAddress("wglCreateContextAttribsARB");
    if (!wglCreateContextAttribsARB) {
        printf("no attribs entry\n");
        return 1;
    }
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(dummy);
    ReleaseDC(hwnd, dummy_dc);
    // Real 3.3 core context on the same window.
    HDC dc = GetDC(hwnd);
    if (!setup_pixel_format(dc)) {
        printf("pixel-format failed\n");
        return 1;
    }
    const int attribs[] = {WGL_CONTEXT_MAJOR_VERSION_ARB, 3, WGL_CONTEXT_MINOR_VERSION_ARB, 3,
                           WGL_CONTEXT_PROFILE_MASK_ARB, WGL_CONTEXT_CORE_PROFILE_BIT_ARB, 0};
    HGLRC gl = wglCreateContextAttribsARB(dc, NULL, attribs);
    if (!gl || !wglMakeCurrent(dc, gl)) {
        printf("core-context failed\n");
        return 1;
    }
    const char *ver = (const char *)glGetString(0x1F02 /* GL_VERSION */);
    printf("gl=%s\n", ver ? ver : "?");
// Load entry points.
#define LOADGL(t, n) t n = (t)wglGetProcAddress(#n)
    LOADGL(PFNGLCREATESHADERPROC, glCreateShader);
    LOADGL(PFNGLSHADERSOURCEPROC, glShaderSource);
    LOADGL(PFNGLCOMPILESHADERPROC, glCompileShader);
    LOADGL(PFNGLCREATEPROGRAMPROC, glCreateProgram);
    LOADGL(PFNGLATTACHSHADERPROC, glAttachShader);
    LOADGL(PFNGLLINKPROGRAMPROC, glLinkProgram);
    LOADGL(PFNGLUSEPROGRAMPROC, glUseProgram);
    LOADGL(PFNGLGENBUFFERSPROC, glGenBuffers);
    LOADGL(PFNGLBINDBUFFERPROC, glBindBuffer);
    LOADGL(PFNGLBUFFERDATAPROC, glBufferData);
    LOADGL(PFNGLGENVERTEXARRAYSPROC, glGenVertexArrays);
    LOADGL(PFNGLBINDVERTEXARRAYPROC, glBindVertexArray);
    LOADGL(PFNGLENABLEVERTEXATTRIBARRAYPROC, glEnableVertexAttribArray);
    LOADGL(PFNGLVERTEXATTRIBPOINTERPROC, glVertexAttribPointer);
#undef LOADGL
    if (!glCreateShader || !glCreateProgram || !glGenBuffers || !glGenVertexArrays) {
        printf("entry-load failed\n");
        return 1;
    }
    unsigned vs = make_shader(glCreateShader, glShaderSource, glCompileShader, GL_VERTEX_SHADER,
                              vs_src);
    unsigned fs = make_shader(glCreateShader, glShaderSource, glCompileShader,
                              GL_FRAGMENT_SHADER, fs_src);
    unsigned prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    static const float verts[] = {-0.6f, -0.4f, 1, 0, 0, 0.6f, -0.4f, 0, 1,
                                  0, 0.0f, 0.6f,  0, 0, 1};
    unsigned vao = 0, vbo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, 0x1406 /* GL_FLOAT */, 0, 5 * sizeof(float), (const void *)0);
    glEnableVertexAttribArray(1);
    glVertexAttribPointer(1, 3, 0x1406 /* GL_FLOAT */, 0, 5 * sizeof(float),
                          (const void *)(2 * sizeof(float)));
    // RenderDoc in-app API (absent standalone — renders normally then).
    {
        HMODULE mod = GetModuleHandleA("renderdoc.dll");
        if (mod) {
            pRENDERDOC_GetAPI get = (pRENDERDOC_GetAPI)GetProcAddress(mod, "RENDERDOC_GetAPI");
            if (get && get(eRENDERDOC_API_Version_1_6_0, (void **)&rdoc) != 1) {
                rdoc = NULL;
            }
        }
    }
    printf("rdoc=%d\n", rdoc != NULL);
    int frames = 0;
    MSG msg;
    while (frames < FRAMES) {
        while (PeekMessage(&msg, NULL, 0, 0, PM_REMOVE)) {
            if (msg.message == WM_QUIT) {
                frames = FRAMES;
                break;
            }
            TranslateMessage(&msg);
            DispatchMessage(&msg);
        }
        if (rdoc && frames == CAP_FRAME) {
            rdoc->SetActiveWindow(gl, hwnd);
            rdoc->TriggerCapture();
            printf("cap-triggered capturing=%u\n", (unsigned)rdoc->IsFrameCapturing());
        }
        glClearColor(0.1f, 0.2f, 0.3f, 1.0f);
        glClear(0x00004000 /* GL_COLOR_BUFFER_BIT */);
        glUseProgram(prog);
        glBindVertexArray(vao);
        glDrawArrays(0x0004 /* GL_TRIANGLES */, 0, 3);
        SwapBuffers(dc);
        frames++;
    }
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(gl);
    ReleaseDC(hwnd, dc);
    DestroyWindow(hwnd);
    printf("frames=%d\n", frames);
    return 0;
}
