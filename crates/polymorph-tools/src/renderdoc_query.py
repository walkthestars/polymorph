# Shipped RenderDoc query driver (chunk 113, N1b) — single file,
# mode-dispatched, include_str!-embedded at compile time (always
# available to installed binaries, version-locked with the tool).
# Runs inside `qrenderdoc --script` (embedded 3.8 runtime — host
# CPython can NOT import the module, proven in 113a recon).
#
# Env: RDOC_MODE in {draws, state, diff}, RDOC_FILE (+ RDOC_FILE_B for
# diff, RDOC_EVENT for state), RDOC_OUT (required: script stdout is
# invisible under qrenderdoc — file output is the only channel).
# Contract: ALWAYS writes RDOC_OUT {"ok": true, ...} /
# {"ok": false, "error": ...}, ALWAYS quits the UI after (or the
# headed process hangs).
import json
import os

OUT = os.environ.get('RDOC_OUT', '')
MODE = os.environ.get('RDOC_MODE', 'draws')
result = {'ok': False}


def finish():
    try:
        with open(OUT, 'w') as f:
            json.dump(result, f)
    except Exception:
        pass
    try:
        from PySide2.QtWidgets import QApplication
        app = QApplication.instance()
        if app is not None:
            app.quit()
    except Exception:
        pass


def fail(msg):
    result['ok'] = False
    result['error'] = str(msg)[:500]


try:
    import renderdoc
    HAVE_MODULE = True
    IMPORT_ERR = None
except Exception as e:
    HAVE_MODULE = False
    IMPORT_ERR = e


DC = 0
OK = None
if HAVE_MODULE:
    DC = int(renderdoc.ActionFlags.Drawcall)
    OK = renderdoc.ResultCode.Succeeded


def open_replay(path):
    cap = renderdoc.OpenCaptureFile()
    try:
        res = cap.OpenFile(path, '', None)
        if res != OK:
            raise RuntimeError('open %s: %s' % (path, res))
        opts = renderdoc.ReplayOptions()
        status, controller = cap.OpenCapture(opts, None)
        if status != OK:
            raise RuntimeError('replay %s: %s' % (path, status))
        return cap, controller
    except Exception:
        try:
            cap.Shutdown()
        except Exception:
            pass
        raise


def close(cap, controller):
    try:
        controller.Shutdown()
    except Exception:
        pass
    try:
        cap.Shutdown()
    except Exception:
        pass


def walk(action, out):
    flags = int(action.flags)
    try:
        name = str(action.customName or '')
    except Exception:
        name = ''
    out.append({
        'event': int(action.eventId),
        'action': int(action.actionId),
        'flags': flags,
        'draw': bool(flags & DC),
        'indices': int(action.numIndices),
        'instances': int(action.numInstances),
        'name': name[:120],
    })
    try:
        kids = list(action.children)
    except Exception:
        kids = []
    for c in kids:
        walk(c, out)


def draw_key(d):
    return (d['event'], d['flags'], d['indices'], d['instances'])


def collect(path):
    cap, controller = open_replay(path)
    events = []
    try:
        for a in controller.GetRootActions():
            walk(a, events)
        return cap, controller, events
    except Exception:
        close(cap, controller)
        raise


def state_for(controller, event):
    controller.SetFrameEvent(int(event), True)
    ps = controller.GetPipelineState()
    nview = 0
    try:
        for i in range(8):
            try:
                ps.GetViewport(i)
                nview += 1
            except Exception:
                break
    except Exception:
        nview = 0
    try:
        ninputs = len(ps.GetVertexInputs())
    except Exception:
        ninputs = 0
    try:
        noutputs = len(ps.GetOutputTargets())
    except Exception:
        noutputs = 0
    try:
        topo = str(ps.GetPrimitiveTopology())
    except Exception as e:
        topo = 'ERR %r' % (e,)
    return {
        'event': int(event),
        'topology': topo,
        'viewports': nview,
        'vertex_inputs': ninputs,
        'output_targets': noutputs,
    }


if not HAVE_MODULE:
    fail('no renderdoc module: %r' % (IMPORT_ERR,))
elif MODE == 'draws':
    try:
        cap, controller, events = collect(os.environ.get('RDOC_FILE', ''))
        try:
            result['ok'] = True
            result['events'] = events
            result['count'] = len(events)
            result['draws'] = sum(1 for e in events if e['draw'])
        finally:
            close(cap, controller)
    except Exception as e:
        fail('%s' % (e,))
elif MODE == 'state':
    try:
        cap, controller, events = collect(os.environ.get('RDOC_FILE', ''))
        try:
            result['ok'] = True
            result['state'] = state_for(controller, os.environ.get('RDOC_EVENT', '0'))
        finally:
            close(cap, controller)
    except Exception as e:
        fail('%s' % (e,))
elif MODE == 'diff':
    try:
        cap_a, ctl_a, ev_a = collect(os.environ.get('RDOC_FILE', ''))
        try:
            cap_b, ctl_b, ev_b = collect(os.environ.get('RDOC_FILE_B', ''))
            try:
                ka = {draw_key(d) for d in ev_a}
                kb = {draw_key(d) for d in ev_b}
                result['ok'] = True
                result['added'] = sorted([list(k) for k in (kb - ka)])
                result['removed'] = sorted([list(k) for k in (ka - kb)])
                result['count_a'] = len(ev_a)
                result['count_b'] = len(ev_b)
            finally:
                close(cap_b, ctl_b)
        finally:
            close(cap_a, ctl_a)
    except Exception as e:
        fail('%s' % (e,))
else:
    fail('unknown RDOC_MODE %r' % (MODE,))

finish()
