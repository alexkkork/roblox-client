"""Offline RunService tests, including real persistent Luau scheduling."""
import copy
import json
import os
from pathlib import Path
import sys
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
import local_runtime
import persistent_runtime

if os.environ.get('LUAU_TEST_HOST'):
    local_runtime.RUST_HOST = Path(os.environ['LUAU_TEST_HOST'])
    if not local_runtime.RUST_HOST.is_file():
        raise RuntimeError('LUAU_TEST_HOST must point to a completed build')
    persistent_runtime.RUST_HOST = local_runtime.RUST_HOST

SNAPSHOT = json.loads((ROOT / 'tests/fixtures/world_snapshot.json').read_text())


def missing_service_snapshot():
    snapshot = copy.deepcopy(SNAPSHOT)
    key = snapshot['services'].pop('RunService')
    snapshot['nodes'] = [node for node in snapshot['nodes'] if node['id'] != key]
    return snapshot


class RunServiceTests(unittest.TestCase):
    def host(self, snapshot=None):
        host = persistent_runtime.PersistentLuauHost(
            snapshot if snapshot is not None else missing_service_snapshot())
        self.addCleanup(host.close)
        return host

    def execute(self, host, source):
        result = host.call({'op': 'execute', 'source': source, 'timeout_ms': 200})
        self.assertTrue(result['ok'], result)
        return result

    def test_missing_service_has_stable_identity_and_client_modes(self):
        host = self.host()
        result = self.execute(host, '''
heldRunService=game:GetService("RunService")
return heldRunService==game:FindService("RunService"),
 heldRunService.Parent==game,heldRunService:IsA("RunService"),
 heldRunService:IsClient(),heldRunService:IsServer(),
 heldRunService:IsRunning(),heldRunService:IsStudio()
''')
        self.assertEqual(result['values'], [True, True, True, True, False, True, False])
        host.call({'op': 'update', 'delta': {'version': 1, 'delta': True, 'actions': []}, 'timeout_ms': 50})
        self.assertEqual(self.execute(host, 'return heldRunService==game:GetService("RunService")')['values'], [True])

    def test_received_service_is_reused_without_duplicate(self):
        host = self.host(copy.deepcopy(SNAPSHOT))
        result = self.execute(host, '''
local r=game:GetService("RunService")
local count=0
for _,v in game:GetDescendants() do if v:IsA("RunService") then count+=1 end end
return r.Name,count,r==game:FindService("RunService")
''')
        self.assertEqual(result['values'], ['Run Service', 1, True])

    def test_heartbeat_wait_resumes_across_polls(self):
        host = self.host()
        result = self.execute(host, 'return game:GetService("RunService").Heartbeat:Wait()')
        if not result['completed']:
            time.sleep(.035)
            poll = host.call({'op': 'poll', 'timeout_ms': 100})
            result = next(r for r in poll['completed_jobs'] if r['job_id'] == result['job_id'])
        self.assertTrue(result['ok'], result)
        self.assertEqual(len(result['values']), 1)
        self.assertGreater(result['values'][0], 0)

    def test_callbacks_once_disconnect_and_no_catchup_burst(self):
        host = self.host()
        self.execute(host, '''
calls=0;onceCalls=0
local heartbeat=game:GetService("RunService").Heartbeat
connection=heartbeat:Connect(function(dt)assert(dt>0);calls+=1;print("heartbeat") end)
heartbeat:Once(function()onceCalls+=1 end)
''')
        time.sleep(.1)
        poll = host.call({'op': 'poll', 'timeout_ms': 100})
        self.assertFalse(poll.get('task_errors'), poll)
        # Inspect this poll's callbacks, not a count across separate RPCs: the
        # subsequent execute can legitimately produce another real-time tick.
        self.assertEqual(poll['output'].splitlines(), ['heartbeat'])
        result = self.execute(host, 'connection:Disconnect();return calls,onceCalls,connection.Connected')
        stopped_at = result['values'][0]
        self.assertGreaterEqual(stopped_at, 1)
        self.assertEqual(result['values'][1:], [1, False])
        time.sleep(.035)
        host.call({'op': 'poll', 'timeout_ms': 100})
        self.assertEqual(self.execute(host, 'return calls')['values'], [stopped_at])

    def test_heartbeat_is_not_starved_by_render_frames(self):
        # Regression: RenderStepped queued events before the Heartbeat check,
        # so with a RenderStepped connection Heartbeat never fired.
        host = self.host()
        self.execute(host, '''
local r=game:GetService("RunService")
renders=0;beats=0;steps=0;pre=0;post=0;lastTime=0;order={}
r.RenderStepped:Connect(function(dt)assert(dt>0);renders+=1 end)
r.Stepped:Connect(function(t,dt)assert(dt>0 and t>lastTime);lastTime=t;steps+=1;if #order<4 then table.insert(order,"Stepped") end end)
r.PreSimulation:Connect(function(dt)assert(dt>0);pre+=1 end)
r.PostSimulation:Connect(function(dt)assert(dt>0);post+=1 end)
r.Heartbeat:Connect(function(dt)assert(dt>0);beats+=1;if #order<4 then table.insert(order,"Heartbeat") end end)
''')
        for _ in range(6):
            time.sleep(.035)
            poll = host.call({'op': 'poll', 'timeout_ms': 100})
            self.assertFalse(poll.get('task_errors'), poll)
        renders, beats, steps, pre, post, first = self.execute(
            host, 'return renders,beats,steps,pre,post,order[1]')['values']
        self.assertGreaterEqual(renders, 1)
        self.assertGreaterEqual(beats, 1)
        self.assertEqual((steps, pre, post), (beats, beats, beats))
        self.assertEqual(first, 'Stepped')

    def test_oneshot_wait_uses_same_heartbeat_binding(self):
        result = local_runtime.run_source('''
local r=game:GetService("RunService")
local dt=r.Heartbeat:Wait()
return r:IsClient(),dt>0
''', world_snapshot=missing_service_snapshot(), timeout_ms=500)
        self.assertTrue(result['ok'], result)
        self.assertEqual(result['values'], [True, True])
        services = [entry['node'] for entry in result['mutations']
                    if entry['kind'] == 'create'
                    and entry['node']['class_name'] == 'RunService']
        self.assertEqual(len(services), 1)
        self.assertFalse(services[0]['server_owned'])


if __name__ == '__main__':
    unittest.main()
