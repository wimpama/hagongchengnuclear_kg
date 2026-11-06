import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import { Resvg } from '@resvg/resvg-js'
import { renderPptx } from './pptx.mjs'
import { createFixture } from './pptx-fixture.mjs'
import { preparePptxObjects } from './pptx-objects.mjs'

async function oleFixture({ preview = true, fallback = false, icon = false } = {}) {
  const archive = unzipSync(await createFixture())
  const name = 'ppt/slides/slide3.xml' // First display page, despite the part name.
  const xml = strFromU8(archive[name])
  const picture = xml.match(/<p:pic\b[\s\S]*?<\/p:pic>/)[0]
  const frame = `<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="89" name="散射方程预览"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm rot="0"><a:off x="914400" y="914400"/><a:ext cx="7315200" cy="3657600"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/presentationml/2006/ole"><p:oleObj progId="Equation.3" showAsIcon="${icon ? 1 : 0}"><p:embed/>${preview ? picture : ''}</p:oleObj></a:graphicData></a:graphic></p:graphicFrame>`
  archive[name] = strToU8(xml.replace(picture, fallback
    ? `<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice Requires="p">${frame}</mc:Choice><mc:Fallback>${picture}</mc:Fallback></mc:AlternateContent>` : frame))
  return zipSync(archive)
}

test('OLE uses the saved picture at its frame bounds; compatibility fallback and real page progress are preserved', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-ole-'))
  try {
    for (const fallback of [false, true]) {
      const source = path.join(root, `ole-${fallback}.pptx`)
      const original = await oleFixture({ fallback })
      await writeFile(source, original)
      const events = []
      const { pages } = await renderPptx(source, path.join(root, `pages-${fallback}`), 200, event => events.push(event))
      assert.equal(pages.length, 3)
      assert.deepEqual(events.map(e => [e.completed, e.total]), [[0, 3], [1, 3], [2, 3], [3, 3]])
      const image = await readFile(path.join(root, `pages-${fallback}`, '1.png'))
      const pixels = new Resvg(`<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><image width="160" height="90" href="data:image/png;base64,${image.toString('base64')}"/></svg>`).render().pixels
      let red = 0
      for (let i = 0; i < pixels.length; i += 4) if (pixels[i] > 150 && pixels[i+1] < 90 && pixels[i+2] < 90) red++
      assert.ok(red > 2000, 'embedded preview pixels were lost')
      assert.ok((await readFile(source)).equals(Buffer.from(original)), 'source must remain unchanged')
    }
    // The default CLI remains a single JSON; the backend opts into NDJSON.
    const cli = spawnSync(process.execPath, ['pptx.mjs', path.join(root, 'ole-false.pptx'), path.join(root, 'cli'), '200', '--progress'], { encoding: 'utf8', windowsHide: true })
    assert.equal(cli.status, 0, cli.stderr)
    const lines = cli.stdout.trim().split('\n').map(line => JSON.parse(line))
    assert.equal(lines[1].completed, 1)
    assert.equal(lines.at(-1).pages.length, 3)
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('missing OLE content preview and icon-only objects fail with actual page and object name', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-missing-preview-'))
  try {
    for (const options of [{ preview: false }, { icon: true }]) {
      const source = path.join(root, 'missing.pptx')
      await writeFile(source, await oleFixture(options))
      await assert.rejects(renderPptx(source, path.join(root, 'pages')), /第 1 页OLE 嵌入对象「散射方程预览」.*内容预览图/)
    }
    const archive = unzipSync(await oleFixture())
    const rels = 'ppt/slides/_rels/slide3.xml.rels'
    archive[rels] = strToU8(strFromU8(archive[rels]).replace(/Target="[^\"]*\.png"/g, 'Target="https://invalid.example/preview.png" TargetMode="External"'))
    assert.throws(() => preparePptxObjects(zipSync(archive)), /第 1 页.*预览图未嵌入或已损坏/)
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('code renderer preserves real slide order, CJK text, embedded image and native chart', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-'))
  try {
    const source = path.join(root, 'course.pptx')
    await writeFile(source, await createFixture())
    const { pages } = await renderPptx(source, path.join(root, 'pages'))
    assert.equal(pages.length, 3)
    assert.equal(pages[0].text, '')
    assert.match(pages[1].text, /核反应截面 NEUTRON CROSS SECTION/)
    assert.match(pages[2].text, /CHART PAGE/)
    assert.deepEqual(pages.map(page => page.page), [1, 2, 3])
    for (const page of pages) {
      const png = await readFile(path.join(root, 'pages', page.imageFile))
      assert.equal(png.subarray(0, 8).toString('hex'), '89504e470d0a1a0a')
      assert.ok(png.length > 1000)
    }
    // Verify actual pixels, not a generic image placeholder.
    const first = await readFile(path.join(root, 'pages', '1.png'))
    const pixels = new Resvg(`<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><image width="160" height="90" href="data:image/png;base64,${first.toString('base64')}"/></svg>`).render().pixels
    let red = 0
    for (let i = 0; i < pixels.length; i += 4) if (pixels[i] > 150 && pixels[i + 1] < 90 && pixels[i + 2] < 90) red++
    assert.ok(red > 2000, 'embedded source image was lost')
    await assert.rejects(renderPptx(source, path.join(root, 'limited'), 2), /页数/)
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('an equation outside a slide text paragraph is explicitly rejected', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-math-'))
  try {
    const zip = unzipSync(await createFixture())
    const name = 'ppt/slides/slide1.xml'
    const xml = strFromU8(zip[name]).replace('</p:spTree>', '<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:r><m:t>x</m:t></m:r></m:oMath></p:spTree>')
    zip[name] = strToU8(xml)
    const source = path.join(root, 'math.pptx')
    await writeFile(source, zipSync(zip))
    await assert.rejects(renderPptx(source, path.join(root, 'pages')), /公式.*文本段落/)
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('navigation hyperlinks render, real linked pictures require detection and explicit skipping', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-external-'))
  try {
    const zip = unzipSync(await createFixture())
    const slide = 'ppt/slides/slide1.xml'
    const rels = 'ppt/slides/_rels/slide1.xml.rels'
    // A real hyperlink paints an SVG anchor but needs no image download.
    // Add a hyperlink run rather than depending on run-property serialization.
    zip[slide] = strToU8(strFromU8(zip[slide]).replace('</a:p>', '<a:r><a:rPr><a:hlinkClick r:id="nav"/></a:rPr><a:t>网页链接</a:t></a:r></a:p>'))
    zip[rels] = strToU8(strFromU8(zip[rels]).replace('</Relationships>', '<Relationship Id="nav" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/></Relationships>'))
    const source = path.join(root, 'hyperlink.pptx')
    await writeFile(source, zipSync(zip))
    const result = await renderPptx(source, path.join(root, 'hyperlink-pages'))
    assert.match(result.pages[1].text, /网页链接/)

    // Add an external-only picture alongside existing embedded media.
    zip[slide] = strToU8(strFromU8(zip[slide]).replace('</p:spTree>', '<p:pic><p:nvPicPr><p:cNvPr id="321" name="linked"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:link="externalPicture"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="rect"/></p:spPr></p:pic></p:spTree>'))
    zip[rels] = strToU8(strFromU8(zip[rels]).replace('</Relationships>', '<Relationship Id="externalPicture" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="https://unavailable.invalid/test.jpg" TargetMode="External"/></Relationships>'))
    const linked = path.join(root, 'linked.pptx')
    const cleaned = path.join(root, 'cleaned.pptx')
    await writeFile(linked, zipSync(zip))
    const python = process.env.KB_PYTHON_BIN || (process.platform === 'win32' ? 'py' : 'python3')
    const args = process.platform === 'win32' && !process.env.KB_PYTHON_BIN ? ['-3'] : []
    const inspect = mode => {
      const output = spawnSync(python, [...args, path.resolve('pptx_external.py'), linked, cleaned, mode], { encoding: 'utf8', windowsHide: true })
      assert.equal(output.status, 0, output.stderr)
      const report = JSON.parse(output.stdout)
      assert.equal(report.error, undefined)
      return report.externalResources
    }
    assert.deepEqual(inspect('inspect'), { count: 1, pages: [2], kinds: ['image'] })
    assert.deepEqual(inspect('skip'), { count: 1, pages: [2], kinds: ['image'] })
    const sanitized = await renderPptx(cleaned, path.join(root, 'clean-pages'))
    assert.equal(sanitized.pages.length, 3)
    assert.match(sanitized.pages[1].text, /核反应截面/)
    assert.ok((await readFile(path.join(root, 'clean-pages', '2.png'))).length > 1000)
  } finally { await rm(root, { recursive: true, force: true }) }
})
