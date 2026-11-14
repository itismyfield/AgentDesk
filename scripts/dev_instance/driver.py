"""One allowed-bot Discord turn for an admitted instance; no runtime lifecycle authority."""
import argparse
from contextlib import contextmanager
import importlib
import json
import math
from pathlib import Path
import sys
import time
import uuid

from . import discord_rest
from .driver_admission import IdentityRefused, admit
from .files import InstanceError, safe_absolute
from .manifest import read_json

MAX_OBSERVE_SECONDS = 600


@contextmanager
def load_assertions(bots):
    module = importlib.import_module('scripts.e2e.tui_relay.assertions')
    previous = module.OUR_BOT_ID, module.PROVIDER_BOT_IDS
    module.OUR_BOT_ID = bots['driver']
    module.PROVIDER_BOT_IDS = {role:bots[role] for role in ('claude','codex')}
    try:yield module
    finally:module.OUR_BOT_ID, module.PROVIDER_BOT_IDS = previous


def admission_witness(root, provider, parent, thread, message, author):
    try:
        row = read_json(root/'runtime/discord_inflight'/provider/(thread+'.json'))
        if (row.get('provider') != provider or row.get('channel_id') != int(thread)
                or row.get('logical_channel_id') != int(parent) or row.get('thread_id') != int(thread)
                or row.get('request_owner_user_id') != int(author)
                or type(row.get('user_msg_id')) is not int or row['user_msg_id'] != int(message)
                or type(row.get('source_message_ids')) is not list
                or row['source_message_ids'] not in ([],[int(message)])
                or not all(type(row.get(key)) is str and row[key] for key in ('turn_nonce','session_id','output_path'))
                or not safe_absolute(row['output_path']).is_relative_to(root)):
            return None
        return {'lineage':'direct_message' if not row['source_message_ids'] else 'source_lineage',
                **{key:row[key] for key in ('provider','channel_id','logical_channel_id','thread_id','request_owner_user_id',
                                        'user_msg_id','source_message_ids','turn_nonce','session_id','output_path')}}
    except (InstanceError,OSError,ValueError,TypeError):
        return None


def run(base, instance_id, provider, timeout, *, identity_request=None, rest_factory=None):
    if provider not in ('claude','codex') or type(timeout) not in (int,float) or not math.isfinite(timeout) or not 0<timeout<=600:
        return {'status':'UNEVALUABLE(identity)','transmissions':0}
    deadline = time.monotonic()+timeout
    try:
        admitted = admit(base,instance_id,deadline,request=identity_request)
    except IdentityRefused:
        return {'status':'UNEVALUABLE(identity)','transmissions':0}
    deadline = min(deadline,time.monotonic()+MAX_OBSERVE_SECONDS)
    manifest = admitted.manifest
    summary = {'status':'UNEVALUABLE(admission)','input_source':'allowed_bot','human_input_coverage':False,
               'instance_id':instance_id,'nonce':admitted.run['nonce'],'head':manifest['head'],
               'authors':manifest['profile']['bots'],'transmissions':0,'transmission_attempts':0}
    try:
        with load_assertions(manifest['profile']['bots']) as assertions:
            client = (rest_factory or discord_rest.Client)(admitted,deadline)
            parent = manifest['profile']['channels'][('claude','codex').index(provider)]
            run_id = str(uuid.uuid4())
            summary['transmission_attempts']=1
            thread = client.create_thread(parent,'adk-test-'+run_id)
            summary.update(thread_id=thread,transmissions=1)
            marker = 'ADK_INSTANCE_'+uuid.uuid4().hex
            summary['transmission_attempts']=2
            message = client.send_prompt(thread,f'DISPATCH:{run_id}\nReply with exactly: {marker}')
            summary.update(message_id=message,transmissions=2)
            window = assertions.Window(setup_marker_id=message)
            witness = None
            while time.monotonic()<deadline:
                witness = witness or admission_witness(Path(manifest['root']),provider,parent,thread,message,manifest['profile']['bots']['driver'])
                for row in client.messages(thread,message):window.add(row)
                try:
                    assertions.provider_text_present(window,needle=marker,author_id=manifest['profile']['bots'][provider])
                    if witness:
                        summary.update(status='OBSERVED',admission=witness,
                            response_ids=[str(row['id']) for row in window.messages
                                          if str(row.get('author',{}).get('id'))==manifest['profile']['bots'][provider]
                                          and marker in (row.get('content') or '')])
                        return summary
                except assertions.AssertionError:pass
                time.sleep(min(1.0,max(0,deadline-time.monotonic())))
    except (InstanceError,OSError,ValueError,TypeError,KeyError):
        summary.update(status='UNEVALUABLE(observation)',transmissions=None)
    # No DELETE/archive: public threads auto-archive without broader bot permissions.
    return summary


class Parser(argparse.ArgumentParser):
    def error(self,message):
        raise IdentityRefused('invalid instance Driver arguments')


def main(argv=None):
    parser = Parser(description='Observe one isolated instance Discord turn')
    parser.add_argument('--instance',required=True)
    parser.add_argument('--provider',choices=('claude','codex'),required=True)
    parser.add_argument('--timeout',type=float,default=180)
    try:
        args = parser.parse_args(argv)
        result=run(safe_absolute(Path.home()/'.adk/dev/instances'),args.instance,args.provider,args.timeout)
    except (InstanceError,OSError,ValueError):result={'status':'UNEVALUABLE(identity)','transmissions':0}
    print(json.dumps(result,sort_keys=True))
    return 0 if result['status']=='OBSERVED' else 3


if __name__=='__main__':sys.exit(main())
