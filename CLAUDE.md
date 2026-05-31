# TouchOSC `.tosc` file format

TouchOSC is made by **Hexler** (hexler.net). The `.tosc` format is their proprietary
file format for TouchOSC 1.x (released ~2021). The older format was `.touchosc`
(ZIP-based, different XML schema).

## File encoding

`.tosc` files are **raw zlib-compressed XML** — NOT ZIP, NOT plain XML.

```python
# Write
compressed = zlib.compress(xml.encode('utf-8'))
with open('file.tosc', 'wb') as f:
    f.write(compressed)

# Read
xml = zlib.decompress(open('file.tosc', 'rb').read()).decode('utf-8')
```

## XML root

```xml
<?xml version='1.0' encoding='UTF-8'?>
<lexml version='4'>
  <node ID='uuid' type='GROUP'>...</node>
</lexml>
```

- Root element: `<lexml version='4'>` (version **4**, not 3)
- The top-level child is a `GROUP` node acting as the canvas/page

## Node structure

Every control is a `<node ID='uuid' type='TYPE'>` with up to four sections:

```xml
<node ID='...' type='BUTTON'>
  <properties>...</properties>
  <values>...</values>
  <messages>...</messages>   <!-- interactive nodes only -->
  <children>...</children>   <!-- container nodes only -->
</node>
```

## Property syntax

Properties use `<key>` + `<value>` children, **not** `name=` attributes:

```xml
<property type='b'><key><![CDATA[visible]]></key><value>1</value></property>
<property type='i'><key><![CDATA[textSize]]></key><value>18</value></property>
<property type='f'><key><![CDATA[cornerRadius]]></key><value>2</value></property>
<property type='s'><key><![CDATA[name]]></key><value><![CDATA[my_btn]]></value></property>
<property type='c'><key><![CDATA[color]]></key><value><r>0.06</r><g>0.47</g><b>0.83</b><a>1</a></value></property>
<property type='r'><key><![CDATA[frame]]></key><value><x>20</x><y>80</y><w>200</w><h>60</h></value></property>
```

| Type | Meaning | Value format |
|---|---|---|
| `b` | bool | `<value>1</value>` or `<value>0</value>` (integers, not true/false) |
| `i` | integer | `<value>NUMBER</value>` |
| `f` | float | `<value>NUMBER</value>` |
| `s` | string | `<value><![CDATA[text]]></value>` |
| `c` | color RGBA | `<value><r>R</r><g>G</g><b>B</b><a>A</a></value>` (floats 0–1) |
| `r` | rect | `<value><x>X</x><y>Y</y><w>W</w><h>H</h></value>` (pixels) |

## Values section

Holds runtime state and defaults for a node's variables:

```xml
<values>
  <value>
    <key><![CDATA[x]]></key>
    <locked>0</locked>
    <lockedDefaultCurrent>0</lockedDefaultCurrent>  <!-- 1 = lock to default (static) -->
    <default><![CDATA[0]]></default>
    <defaultPull>0</defaultPull>
  </value>
</values>
```

- BUTTON / FADER: variables `x` (0–1 position) and `touch`
- TEXT / LABEL: variable `text` (`lockedDefaultCurrent=1` for static display)

## OSC messages section

OSC addresses and arguments are **not** properties — they live in `<messages>`:

```xml
<messages><osc>
  <enabled>1</enabled>
  <send>1</send>        <!-- sends OSC on trigger -->
  <receive>1</receive>  <!-- receives OSC to update state -->
  <feedback>0</feedback>
  <connections>11111</connections>
  <triggers>
    <trigger><var><![CDATA[x]]></var><condition>RISE</condition></trigger>
  </triggers>
  <path>
    <partial><type>CONSTANT</type><conversion>STRING</conversion>
      <value><![CDATA[/metronome/bpm]]></value>
      <scaleMin>0</scaleMin><scaleMax>1</scaleMax></partial>
  </path>
  <arguments>
    <!-- constant value -->
    <partial><type>CONSTANT</type><conversion>FLOAT</conversion>
      <value><![CDATA[1]]></value><scaleMin>0</scaleMin><scaleMax>1</scaleMax></partial>
    <!-- variable (e.g. fader position scaled to real range) -->
    <partial><type>VAR</type><var><![CDATA[x]]></var><conversion>FLOAT</conversion>
      <scaleMin>20</scaleMin><scaleMax>200</scaleMax></partial>
    <!-- auto-convert incoming float to text (for display labels) -->
    <partial><type>VAR</type><var><![CDATA[text]]></var><conversion>AUTO</conversion>
      <scaleMin>0</scaleMin><scaleMax>1</scaleMax></partial>
  </arguments>
</osc></messages>
```

Trigger conditions: `RISE` (on press/momentary), `ANY` (every change — use for toggles and faders)

## Node types and their properties

Confirmed types: `GROUP`, `BOX`, `BUTTON`, `LABEL`, `TEXT`, `FADER`

**All nodes** need: `background`, `color`, `cornerRadius`, `frame`, `grabFocus`,
`interactive`, `locked`, `name`, `orientation`, `outline`, `outlineStyle`,
`pointerPriority`, `shape`, `visible`

**Text nodes** (TEXT, LABEL) add: `font`, `textAlignH`, `textAlignV`, `textClip`,
`textColor`, `textSize`, `textWrap` (TEXT only), `textLength` (LABEL only)

**BUTTON** adds: `buttonType` (0=momentary, 1=toggle), `press`, `release`, `valuePosition`

**FADER** adds: `bar`, `barColor`, `response`, `centered`
