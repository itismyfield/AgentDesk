"""Sentinel provision→native up→synthetic Discord turn→cooperative down (no live tokens)."""
import io
import json
import os
from pathlib import Path
import unittest
from unittest import mock

from scripts.dev_instance import discord_rest, driver, files, identity
from tests import dev_instance_bound_health_real as native


class SentinelDriverRealTests(unittest.TestCase):
    def test_sentinel_provision_native_up_one_turn_and_down(self):
        from tests.test_dev_instance import profile
        declared=profile();declared['bots']['driver']='900006'
        fixture=native.BoundHealthRealTests(methodName='runTest')
        writer=Path(os.environ['ADK_I3B_CREDENTIAL_WRITER'])
        self.assertTrue(writer.is_absolute() and writer.is_file())
        self.assertEqual(writer.name,'agentdesk')
        with mock.patch.object(native,'profile',return_value=declared):fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        phases=['prepare']
        # Replace this fixture's own synthetic files using the real stdin-only Rust writer.
        for name in native.NAMES:(fixture.root/'credential'/name).unlink()
        with mock.patch.dict(os.environ,{'PATH':str(writer.parent)+':/usr/bin:/bin',
                                         'DISCORD_TOKEN':'SENTINEL_AMBIENT_NEVER_FORWARD'}):
            for name in native.NAMES:
                token=next(token for token,app in fixture.tokens.items() if app==fixture.frame['identities'][name])
                files.write_credential(fixture.root,name,io.BytesIO(token.encode()))
        phases.append('credential(stdin)')
        fixture.start()
        receipt=fixture.wait_for(fixture.receipt,'native current-run readiness',timeout=100)
        fixture.gateway_nonce=receipt['nonce'];phases.append('up(native listener/PG/Herdr)')
        launch=json.loads((fixture.root/'runtime/native-launch.json').read_text())
        for token in fixture.tokens:
            self.assertNotIn(token,json.dumps(launch))
        self.assertNotIn('SYNTHETIC_NEVER_INHERIT',json.dumps(launch))
        self.assertNotIn('SENTINEL_AMBIENT_NEVER_FORWARD',json.dumps(launch))
        request_original=identity.request_json
        def current_request(url,end,token=None):
            return request_original(f'http://127.0.0.1:{fixture.api.server_port}/identity' if token else url,end,token)
        transport_calls=[]
        def client_factory(admitted,deadline):
            def request(role,method,path,body=None):
                # This is an explicit synthetic Discord seam, never a live Gateway claim.
                transport_calls.append({'role':role,'method':method,'path':path})
                parent=declared['channels'][0]
                if path.endswith('/threads'):
                    return {'id':'900008','parent_id':parent,'owner_id':'900006','type':11}
                if method=='POST':
                    marker=body['content'].split('Reply with exactly: ',1)[1]
                    client_factory.marker=marker
                    target=fixture.root/'runtime/discord_inflight/claude'
                    target.mkdir(mode=0o700,parents=True,exist_ok=True)
                    value={'provider':'claude','channel_id':900008,'logical_channel_id':int(parent),
                           'thread_id':900008,'request_owner_user_id':900006,'user_msg_id':900009,
                           'source_message_ids':[],'turn_nonce':'fixture-turn','session_id':'fixture-session',
                           'output_path':str(fixture.root/'outputs/synthetic.jsonl')}
                    path=target/'900008.json';path.write_text(json.dumps(value));path.chmod(0o600)
                    return {'id':'900009','channel_id':'900008','author':{'id':'900006'}}
                return [{'id':'900010','channel_id':'900008','type':0,
                         'author':{'id':'900001','bot':True},'content':client_factory.marker}]
            return discord_rest.Client(admitted,deadline,request=request)
        result=driver.run(fixture.base,fixture.instance_id,'claude',30,
                          identity_request=current_request,rest_factory=client_factory)
        self.assertEqual(result['status'],'OBSERVED',result)
        self.assertEqual(result['admission']['lineage'],'direct_message')
        phases.append('one turn(synthetic REST/admission)')
        fixture.control.write(json.dumps({'root':str(fixture.root),'nonce':receipt['nonce'],'action':'down'})+'\n')
        fixture.control.flush()
        out,err=fixture.wrapper.communicate(timeout=15)
        self.assertEqual(fixture.wrapper.returncode,0,err)
        final=json.loads(out.splitlines()[-1]);self.assertEqual(final['status'],'STOPPED')
        self.assertTrue(final['cooperative_stop']);self.assertTrue(final['gateway_wait_ack'])
        record=fixture.assert_closed();phases.append('down(original wait→Herdr→PG)')
        for token in fixture.tokens:
            self.assertNotIn(token,out+err+json.dumps(result))
        evidence={'source_head':fixture.manifest['head'],'evidence_origin':'sentinel Discord fixture',
                  'live_discord':False,'PG_actual':True,'Herdr_actual':True,'phases':phases,
                  'result':result,'calls':transport_calls,'child_env_argv_secret_free':True,
                  'close_order':record['close_order'],'credential_retained':len(list((fixture.root/'credential').iterdir()))}
        path=Path('reports/i3b/logs/sentinel-dry-run.json');path.write_text(json.dumps(evidence,indent=2)+'\n')
        print('SENTINEL_DRY_RUN '+json.dumps({'phases':phases,'live_discord':False,'PG_actual':True}),flush=True)


if __name__=='__main__':unittest.main()
