"""Rendering asset contracts; fixture bytes and offline VM only."""
import copy
import gzip
import json
import struct
import unittest
from unittest.mock import Mock

from render_assets import RenderAssets, asset_reference, decode_render_mesh
from mesh_asset_decoder import decode_filemesh
from persistent_runtime import PersistentLuauHost
from test_world_snapshot import fixture
from world_snapshot import make_world_snapshot


def mesh_bytes(version='2.00', bones=0, faces=None, lods=()):
    faces=faces or [(0,1,2)]
    if version == '2.00':
        header=struct.pack('<HBBII',12,40,12,3,len(faces))
    elif version in ('3.00','3.01'):
        header=struct.pack('<HBBHHII',16,40,12,4,len(lods),3,len(faces))
    else:
        header=struct.pack('<HHIIHHIHBB',32 if version=='5.00' else 24,0,3,len(faces),len(lods),bones,0,0,0,0)
        if version=='5.00':header+=struct.pack('<II',1,0)
    vertices=b''.join(struct.pack('<8f4b4B',*pos,0,0,1,*uv,0,0,0,0,255,255,255,255)
        for pos,uv in [((0,0,0),(0,0)),((1,0,0),(1,0)),((0,1,0),(0,1))])
    return (b'version '+version.encode()+b'\n'+header+vertices+(b'\0'*24 if bones else b'')+
            b''.join(struct.pack('<3I',*f) for f in faces)+b''.join(struct.pack('<I',i) for i in lods)+b'\0'*(bones*60))


class RenderAssetTests(unittest.TestCase):
    def test_all_supported_binary_versions_retain_uvs_normals_and_rigid_skin_flag(self):
        for version in ('2.00','3.00','3.01','4.00','4.01','5.00'):
            with self.subTest(version=version):
                bones=1 if version in ('4.00','4.01','5.00') else 0
                mesh=decode_render_mesh(mesh_bytes(version,bones))
                self.assertEqual(mesh['positions'],[0,0,0,1,0,0,0,1,0])
                self.assertEqual(mesh['normals'],[0,0,1]*3)
                self.assertEqual(mesh['uv'],[0,0,1,0,0,1])
                self.assertEqual(mesh['indices'],[0,1,2])
                self.assertEqual(mesh['skinned'],bool(bones))

    def test_lod_geometry_is_not_drawn_over_primary_mesh(self):
        mesh=decode_render_mesh(mesh_bytes('4.00',faces=[(0,1,2),(2,1,0)],lods=(0,1,2)))
        self.assertEqual(mesh['indices'],[0,1,2])
        with self.assertRaises(ValueError):decode_render_mesh(mesh_bytes('3.00',lods=(2,1)))

    def test_malformed_counts_indices_and_nonfinite_vertices_are_rejected(self):
        source=mesh_bytes()
        nonfinite=bytearray(source);struct.pack_into('<f',nonfinite,13+12,float('nan'))
        for data in (source[:-1],source+b'bad',bytes(nonfinite),mesh_bytes(faces=[(0,1,999)]),b'version 6.00\n'+b'\0'*100):
            with self.subTest(data=data[:12]):
                with self.assertRaises(ValueError):decode_render_mesh(data)

    def test_compressed_mesh_uses_same_geometry(self):
        self.assertEqual(decode_render_mesh(gzip.compress(mesh_bytes())),decode_render_mesh(mesh_bytes()))

    def test_legacy_position_scale_and_uv_are_retained(self):
        raw=b'version 1.00\n1\n'+b''.join(('[%s,%s,%s]'%v).encode() for v in [(0,0,0),(0,0,1),(0,.25,0),(2,0,0),(0,0,1),(1,.25,0),(0,2,0),(0,0,1),(0,1,0)])
        mesh=decode_render_mesh(raw)
        self.assertEqual(mesh['positions'][3:6],[1,0,0]);self.assertEqual(mesh['uv'][:2],[0,.75])

    def test_empty_lod_table_draws_every_face(self):
        # Dynamic heads (5.00) store a LOD table of 0, 0: one level of every face.
        mesh=decode_render_mesh(mesh_bytes('5.00',1,lods=(0,0)))
        self.assertEqual(mesh['indices'],[0,1,2])

    def test_legacy_mesh_with_crlf_line_endings(self):
        # Old catalog meshes (hair from 2010 and earlier) use Windows line endings.
        lf=b'version 1.00\n1\n'+b''.join(('[%s,%s,%s]'%v).encode() for v in [(0,0,0),(0,0,1),(0,.25,0),(2,0,0),(0,0,1),(1,.25,0),(0,2,0),(0,0,1),(0,1,0)])
        crlf=lf.replace(b'\n',b'\r\n')
        self.assertEqual(decode_render_mesh(crlf),decode_render_mesh(lf))
        self.assertEqual(decode_filemesh(crlf).vertices,decode_filemesh(lf).vertices)

    def test_asset_scope_is_required_before_fetch_and_reset_revokes_it(self):
        fetcher=Mock(return_value=mesh_bytes());store=RenderAssets(fetcher)
        with self.assertRaises(ValueError):store.get('mesh','rbxassetid://7')
        fetcher.assert_not_called()
        store.observe({'parts':[{'mesh':{'id':'rbxassetid://7'}}]})
        first=store.get('mesh','rbxassetid://7');self.assertIs(store.get('mesh','rbxassetid://7'),first)
        self.assertEqual(fetcher.call_count,1)
        with self.assertRaises(ValueError):store.get('texture','rbxassetid://7')
        store.observe({})
        with self.assertRaises(ValueError):store.get('mesh','rbxassetid://7')

    def test_references_cannot_be_arbitrary_urls_or_local_files(self):
        for value in ('file:///etc/passwd','https://evil.test/asset?id=7','http://127.0.0.1/asset?id=7','rbxasset://avatar/../secrets/cookie.png','rbxasset://avatar/file.lua',None):
            with self.assertRaises(ValueError):asset_reference(value)
        self.assertEqual(asset_reference('http://www.roblox.com/asset/?id=007'),'rbxassetid://7')
        self.assertEqual(asset_reference(' http://www.roblox.com/asset/?id=007   '),'rbxassetid://7')
        self.assertEqual(asset_reference('rbxasset://textures/face.png'),'rbxasset://textures/face.png')

    def test_surface_maps_are_scoped_without_allowing_unobserved_assets(self):
        store=RenderAssets(Mock())
        store.observe({'parts':[{'appearance':{field:f'rbxassetid://{i}' for i,field in enumerate(
            ('colorMap','normalMap','roughnessMap','metalnessMap'),10)}}]})
        for i in range(10,14):self.assertIn(('texture',f'rbxassetid://{i}'),store.allowed)
        with self.assertRaises(ValueError):store.get('texture','rbxassetid://99')


class RenderProjectionTests(unittest.TestCase):
    def test_native_projection_includes_avatar_accessories_appearance_and_lighting(self):
        world,_=fixture();snapshot=make_world_snapshot(world,123)
        def node(id,cls,parent,properties,name=None):
            return {'id':id,'class_name':cls,'name':name or cls,'parent':parent,'properties':properties,'origins':{}}
        base={'Size':{'$vector3':[2,2,1]},'CFrame':{'$cframe':{'position':[0,3,0],'rotation':[1,0,0,0,1,0,0,0,1]}},'Material':{'$enum':{'type':'Material','name':'Glass','value':1568}},'Transparency':.25,'LocalTransparencyModifier':.5,'CastShadow':False}
        snapshot['nodes'] += [
            node('torso','Part','1:3',base,'Torso'),
            node('shirt','Shirt','1:3',{'ShirtTemplate':'rbxassetid://100'}),
            node('hat','Accessory','1:3',{}),node('handle','Part','hat',base,'Handle'),
            node('mesh','SpecialMesh','handle',{'MeshId':'rbxassetid://101','TextureId':'rbxassetid://102','MeshType':5,'Scale':{'$vector3':[2,3,4]},'Offset':{'$vector3':[0,.5,0]}}),
            node('decal','Decal','torso',{'Texture':'rbxasset://textures/face.png','Face':5}),
            node('meshpart','MeshPart','1:6',dict(base,MeshContent='rbxassetid://103',TextureContent='rbxassetid://104')),
            node('surface','SurfaceAppearance','meshpart',{'ColorMap':'rbxassetid://105','NormalMap':'rbxassetid://106','RoughnessMap':'rbxassetid://107','MetalnessMap':'rbxassetid://108','Color':{'$color3':[.5,.7,1]},'AlphaMode':1}),
            node('lighting','Lighting',None,{'ClockTime':17,'Brightness':3,'GlobalShadows':True,'FogEnd':500,'EnvironmentDiffuseScale':.4,'EnvironmentSpecularScale':.8})]
        snapshot['services']['Lighting']='lighting'
        host=PersistentLuauHost(snapshot);self.addCleanup(host.close)
        frame=host.call({'op':'render'})['frame'];parts={p['id']:p for p in frame['parts']}
        self.assertEqual(parts['torso']['clothing'],{'shirt':'rbxassetid://100'})
        self.assertEqual(parts['torso']['characterPart'],'Torso')
        self.assertEqual(parts['torso']['localTransparency'],.5)
        self.assertEqual(parts['torso']['material']['$enum']['name'],'Glass')
        self.assertFalse(parts['torso']['castShadow'])
        self.assertTrue(parts['handle']['own']);self.assertEqual(parts['handle']['mesh']['scale']['$vector3'],[2,3,4])
        self.assertEqual(parts['meshpart']['mesh']['id'],'rbxassetid://103')
        self.assertEqual(parts['meshpart']['mesh']['texture'],'rbxassetid://104')
        self.assertEqual(parts['meshpart']['appearance'],{'colorMap':'rbxassetid://105','normalMap':'rbxassetid://106','roughnessMap':'rbxassetid://107','metalnessMap':'rbxassetid://108','color':{'$color3':[.5,.7,1]},'alphaMode':1})
        self.assertEqual(parts['torso']['decals'][0]['texture'],'rbxasset://textures/face.png')
        self.assertEqual(frame['lighting']['ClockTime'],17)
        self.assertEqual(frame['lighting']['EnvironmentDiffuseScale'],.4)
        self.assertEqual(frame['lighting']['EnvironmentSpecularScale'],.8)
        self.assertEqual(host.call({'op':'render'})['frame'],frame)
        self.assertNotIn('script_sources',frame)


if __name__=='__main__':unittest.main()
