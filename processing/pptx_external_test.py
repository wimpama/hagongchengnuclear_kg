import pathlib
import tempfile
import unittest
import zipfile
import xml.etree.ElementTree as ET

from pptx_external import inspect, P, A, R


REL = "http://schemas.openxmlformats.org/package/2006/relationships"


def relations(*items):
    return ('<Relationships xmlns="' + REL + '">' + ''.join(
        f'<Relationship Id="{rid}" Type="{R}/{kind}" Target="{url}"' + (' TargetMode="External"' if external else '') + '/>'
        for rid, kind, url, external in items) + '</Relationships>').encode()


def slide(body):
    return f'<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>'.encode()


class ExternalMediaTest(unittest.TestCase):
    def test_reports_all_used_resources_in_display_order_and_skips_only_linked_shapes(self):
        with tempfile.TemporaryDirectory() as folder:
            source, output = pathlib.Path(folder) / 'source.pptx', pathlib.Path(folder) / 'cleaned.pptx'
            files = {
                'ppt/presentation.xml': f'<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId r:id="s2"/><p:sldId r:id="s1"/></p:sldIdLst></p:presentation>'.encode(),
                'ppt/_rels/presentation.xml.rels': relations(('s1', 'slide', 'slides/slide1.xml', False), ('s2', 'slide', 'slides/slide2.xml', False)),
                'ppt/slides/slide1.xml': slide('<p:pic><p:blipFill><a:blip r:link="linked"/></p:blipFill></p:pic><p:sp><a:t>课程正文</a:t></p:sp>'),
                'ppt/slides/slide2.xml': slide('<p:pic><p:blipFill><a:blip r:embed="embedded" r:link="linked"/></p:blipFill></p:pic><p:sp><a:hlinkClick r:id="nav"/><a:t>正常超链接</a:t></p:sp>'),
                'ppt/slides/_rels/slide1.xml.rels': relations(('linked', 'image', 'file:///unavailable/picture.jpg', True), ('layout', 'slideLayout', '../slideLayouts/slideLayout1.xml', False)),
                'ppt/slides/_rels/slide2.xml.rels': relations(('linked', 'image', 'https://unavailable.invalid/image.png', True), ('embedded', 'image', '../media/local.png', False), ('nav', 'hyperlink', 'https://example.com/', True), ('unused', 'image', 'https://unused.invalid/', True), ('layout', 'slideLayout', '../slideLayouts/slideLayout1.xml', False)),
                'ppt/slideLayouts/slideLayout1.xml': slide('<p:pic><p:blipFill><a:blip r:link="background"/></p:blipFill></p:pic>'),
                'ppt/slideLayouts/_rels/slideLayout1.xml.rels': relations(('background', 'image', 'https://unavailable.invalid/background.png', True)),
                'ppt/media/local.png': b'embedded bytes stay unchanged',
            }
            with zipfile.ZipFile(source, 'w') as z:
                for key, value in files.items():
                    z.writestr(key, value)
            original = source.read_bytes()
            report = inspect(source, output)
            self.assertEqual(report, {'count': 3, 'pages': [1, 2], 'kinds': ['image']})
            self.assertFalse(output.exists(), 'inspection must not write a sanitized copy')
            self.assertEqual(report, inspect(source, output, skip=True))
            self.assertEqual(source.read_bytes(), original)
            self.assertEqual(inspect(output, None)['count'], 0)
            with zipfile.ZipFile(output) as z:
                self.assertEqual(z.read('ppt/media/local.png'), files['ppt/media/local.png'])
                # Display page two's external-only picture is removed completely.
                self.assertFalse(ET.fromstring(z.read('ppt/slides/slide1.xml')).findall(f'.//{{{P}}}pic'))
                image = ET.fromstring(z.read('ppt/slides/slide2.xml')).find(f'.//{{{A}}}blip')
                self.assertEqual(image.attrib, {f'{{{R}}}embed': 'embedded'})
                self.assertIn(b'example.com', z.read('ppt/slides/_rels/slide2.xml.rels'))

    def test_hyperlinks_alone_are_not_external_media(self):
        with tempfile.TemporaryDirectory() as folder:
            source = pathlib.Path(folder) / 'source.pptx'
            with zipfile.ZipFile(source, 'w') as z:
                z.writestr('ppt/presentation.xml', f'<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId r:id="s"/></p:sldIdLst></p:presentation>')
                z.writestr('ppt/_rels/presentation.xml.rels', relations(('s', 'slide', 'slides/slide1.xml', False)))
                z.writestr('ppt/slides/slide1.xml', slide('<p:sp><a:hlinkClick r:id="nav"/></p:sp>'))
                z.writestr('ppt/slides/_rels/slide1.xml.rels', relations(('nav', 'hyperlink', 'https://example.com', True)))
            self.assertEqual(inspect(source, None), {'count': 0, 'pages': [], 'kinds': []})


if __name__ == '__main__':
    unittest.main()
