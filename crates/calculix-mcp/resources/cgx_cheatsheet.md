# CalculiX GraphiX (cgx) batch command reference

Every code block below was run with `cgx -bg` and produced what its `# expect:` lines state
(file, node and element counts, element types, set sizes, bounding box). They are re-run by the
server's integration tests. Syntax lines are quoted from the cgx manual: `<..>` is a parameter,
`[..]` optional, `|` alternatives.

## Rules of the batch run

- The script runs in a fresh run directory; `send` writes its files there, named after the set
  (`send all abq` writes `all.msh`, `send fix abq nam` writes `fix.nam`).
- cgx exits with 0 even when a command fails. Read the `errors` list of the result and check the
  node, element and set counts of every produced file before using it.
- One command per line. Keywords are case-insensitive, parameters (names) are case-sensitive.
  A line starting with `#` is a comment. A line ending in `=` continues on the next line.
- Entity names have at most 8 characters. `!` instead of a name lets cgx choose one.
- Order: points, lines (with divisions), surfaces, bodies, then `elty`, `mesh`, sets, `send`.
- `tra`, `rot` and `scal` used alone only move the VIEW. To transform geometry use
  `move <set> tra|rot|scal ...`, `copy <set> <newset> tra|rot|scal ...` or `swep`.
- Not available in this tool: `sys`, `graph`, `movi`, interactive input (`valu ... ?`), the
  q-commands (`qadd`, `qsur`, ...) and images (`hcpy` writes nothing without a window).
- `read <file>` paths are relative to the script's directory (the run directory). Absolute paths
  and `..` are rejected; the files read are copied into the run directory first.

## Points: pnt

`pnt <name>|'!' <x> <y> <z>` | `pnt <name>|'!' <line> <ratio> <times>` | `pnt <name>|'!' <P1> <P2> <ratio> <times>`

Points define the corners of the geometry. See every example below; `valu` makes them parametric.

## Lines and arcs: line

`line <name>|'!' <p1> <p2> <div>` (straight), `line <name>|'!' <p1> <p2> <cp> <div>` (arc around
centre point `cp`), `line <name> <p1> <p2> <seq> <div>` (spline through the points of a sequence set).
`<div>` is the number of element edges along the line; quadratic elements (`he20`, `qu8`) use
two divisions per element. Opposite edges of a surface must have the same division.

Quarter annulus (inner radius 10, outer 20) from two straight lines and two arcs, meshed with
8-node shells:

```fbd
# expect: all.msh nodes=37 elements=8 S8=8 bbox=0,0,0,20,20,0
pnt pc 0 0 0
pnt pi1 10 0 0
pnt po1 20 0 0
pnt pi2 0 10 0
pnt po2 0 20 0
line l1 pi1 po1 4
line l2 po1 po2 pc 8
line l3 po2 pi2 4
line l4 pi2 pi1 pc 8
surf s1 blend l1 l2 l3 l4
elty all qu8
mesh all
send all abq
```

## Combined lines: lcmb

`lcmb <name>|'!' '+|-' <line> '+|-' <line> ...` joins several lines into one surface edge.
`+`/`-` give the direction of each line.

40 x 10 plate whose bottom edge is made of two lines (4 + 4 divisions against 8 on the top):

```fbd
# expect: all.msh nodes=27 elements=16 S4=16 bbox=0,0,0,40,10,0
pnt p1 0 0 0
pnt pm 15 0 0
pnt p2 40 0 0
pnt p3 40 10 0
pnt p4 0 10 0
line la p1 pm 4
line lb pm p2 4
line l2 p2 p3 2
line l3 p3 p4 8
line l4 p4 p1 2
lcmb c1 + la + lb
surf s1 blend c1 l2 l3 l4
elty all qu4
mesh all
send all abq
```

## Surfaces: surf

`surf <name>|'!' blend <line|lcmb> <line|lcmb> <line|lcmb> [<line|lcmb> [<line|lcmb>]]`
A structured surface has 3, 4 or 5 edges given in order around the boundary. Write `blend`;
without it cgx prints a deprecation warning and assumes it.

## Bodies: body

`body <name>|'!' <bottom-surf> <top-surf>` creates the side surfaces itself when all connecting
lines exist; `body <name> <s1> <s2> <s3> <s4> <s5> [<s6>]` lists all surfaces.

30 x 20 x 10 brick from its bottom and top surfaces and the four vertical lines:

```fbd
# expect: all.msh nodes=105 elements=48 C3D8=48 bbox=0,0,0,30,20,10
pnt p1 0 0 0
pnt p2 30 0 0
pnt p3 30 20 0
pnt p4 0 20 0
pnt p5 0 0 10
pnt p6 30 0 10
pnt p7 30 20 10
pnt p8 0 20 10
line l1 p1 p2 6
line l2 p2 p3 4
line l3 p3 p4 6
line l4 p4 p1 4
line l5 p5 p6 6
line l6 p6 p7 4
line l7 p7 p8 6
line l8 p8 p5 4
line v1 p1 p5 2
line v2 p2 p6 2
line v3 p3 p7 2
line v4 p4 p8 2
surf sb blend l1 l2 l3 l4
surf st blend l5 l6 l7 l8
body b1 sb st
elty all he8
mesh all
send all abq
```

## Sweeps and extrusions: swep

`swep <set> <newset> tra <dx> <dy> <dz> <div> [a]` |
`swep <set> <newset> rot <p1> <p2> <angle> <div>` | `swep <set> <newset> rot x|y|z <angle> <div>` |
`swep <set> <newset> rot <p1> x|y|z <angle> <div>` | `swep <set> <newset> scal <fx> <fy> <fz> <P0> <div>` |
`swep <set> <newset> mir <P1> <P2> <div>` | `swep <set> <newset> nor <dr> <div>`
Sweeping a point gives a line, a line gives a surface, a surface gives a body (or shell elements
give solids). The swept copy is stored in `<newset>`; `<div>` divides the new connecting lines.

Block 100 x 20 x 10 extruded from its base, 20-node hexahedra (2 divisions per element):

```fbd
# expect: all.msh nodes=321 elements=40 C3D20=40 bbox=0,0,0,100,20,10
pnt p1 0 0 0
pnt p2 100 0 0
pnt p3 100 20 0
pnt p4 0 20 0
line l1 p1 p2 20
line l2 p2 p3 4
line l3 p3 p4 20
line l4 p4 p1 4
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 4
elty all he20
mesh all
send all abq
```

90 degree ring segment: a rectangle (radius 10 to 20, height 5) revolved about the z axis:

```fbd
# expect: all.msh nodes=89 elements=8 C3D20=8 bbox=0,0,0,20,20,5
pnt p1 10 0 0
pnt p2 20 0 0
pnt p3 20 0 5
pnt p4 10 0 5
line l1 p1 p2 4
line l2 p2 p3 2
line l3 p3 p4 4
line l4 p4 p1 2
surf s1 blend l1 l2 l3 l4
seta sect s s1
swep sect sect2 rot z 90 8
elty all he20
mesh all
send all abq
```

## Divisions, element types and meshing: div, elty, mesh

`div <line|set> <div>` | `div <set> mult|div <factor>` | `div <set> auto [<node-dist> <angle> <elem-ratio>]`
`elty <set> <type>`: `be2` `be3` `tr3` `tr6` `qu4` `qu8` (shells; `e`/`s`/`c` suffix: plane strain,
plane stress, axisymmetric; `r`: reduced integration), `he8` `he8i` `he8r` `he20` `he20r` `te4`
`te10` (solids). Exported as C3D8, C3D8I, C3D8R, C3D20, C3D20R, C3D4, C3D10, S4, S8, ...
`mesh <set>` meshes everything that has an element type; existing elements are kept.

Parametric block: dimensions in values, all divisions doubled before meshing:

```fbd
# expect: all.msh nodes=525 elements=320 C3D8=320 bbox=0,0,0,50,8,4
valu L 50
valu W 8
valu H 4
pnt p1 0 0 0
pnt p2 L 0 0
pnt p3 L W 0
pnt p4 0 W 0
line l1 p1 p2 10
line l2 p2 p3 2
line l3 p3 p4 10
line l4 p4 p1 2
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 H 2
div all mult 2
elty all he8
mesh all
send all abq
```

Cube with 10-node tetrahedra (built-in tetgen; the element count depends on the mesher):

```fbd
# expect: all.msh C3D10>0 bbox=0,0,0,10,10,10
pnt p1 0 0 0
pnt p2 10 0 0
pnt p3 10 10 0
pnt p4 0 10 0
line l1 p1 p2 4
line l2 p2 p3 4
line l3 p3 p4 4
line l4 p4 p1 4
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 4
elty all te10
mesh all
send all abq
```

## Sets: seta, setr, comp, enq

`seta <set> p|l|c|s|b|n|e|se <name> ...` adds entities (or the content of other sets with `se`);
`seta <set> n <first> - <last> <step>` adds a range of nodes or elements.
`setr <set> p|l|s|b|n|e|se <name> ...` removes entities from a set (they are not deleted).
`comp <set> do` adds everything the entities depend on (points, lines, nodes);
`comp <set> up` adds what depends on them; `comp <set> f` adds the element faces whose nodes
are all in the set; `comp <set> e` adds the elements whose nodes are all in the set.
`enq <set> <newset> rec <x>|_ <y>|_ <z>|_ <tol> a` stores every entity of `<set>` within `<tol>`
of the given coordinates (`_` = any value) in `<newset>`. Start from a set holding only nodes
(`seta nodes n all`), otherwise lines and points are picked too.

Node set of the top face (z = 10) minus the nodes on its edge at x = 0, and the union of both:

```fbd
# expect: topn.nam NTOPN=80
# expect: edge.nam NEDGE=5
# expect: allx.nam NALLX=85
pnt p1 0 0 0
pnt p2 100 0 0
pnt p3 100 20 0
pnt p4 0 20 0
line l1 p1 p2 20
line l2 p2 p3 4
line l3 p3 p4 20
line l4 p4 p1 4
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 4
elty all he20
mesh all
seta nodes n all
enq nodes topn rec _ _ 10 0.01 a
enq nodes edge rec 0 _ 10 0.01 a
setr topn n edge
seta allx se topn edge
send topn abq nam
send edge abq nam
send allx abq nam
send all abq
```

## Transformations: move, copy

`move <set> tra <dx> <dy> <dz>` | `move <set> rot <p1> <p2> <angle>` | `move <set> rot x|y|z <angle>` |
`move <set> scal <f> [<P0>]` | `move <set> scal <fx> <fy> <fz> [<P0>]` | `move <set> mir <P1> <P2>`
`copy <set> <newset> tra|rot|scal|mir ...` takes the same transformations and keeps the original.
Angles are in degrees; `rot z 90` turns about the z axis through the origin, `scal` scales about
the origin unless a point is given.

10 mm cube moved by +5 in x, turned 90 degrees about z, then scaled by 2:

```fbd
# expect: all.msh nodes=27 elements=8 C3D8=8 bbox=-20,10,0,0,30,20
pnt p1 0 0 0
pnt p2 10 0 0
pnt p3 10 10 0
pnt p4 0 10 0
line l1 p1 p2 2
line l2 p2 p3 2
line l3 p3 p4 2
line l4 p4 p1 2
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 2
move all tra 5 0 0
move all rot z 90
move all scal 2
elty all he8
mesh all
send all abq
```

## Merging: merg

`merg p|l|c|s|n|e <set> [<gtol>] ['nolock']` merges coincident points, lines, surfaces, or nodes
and elements of `<set>`. Without merging, touching parts share no nodes and are not connected.

Two touching cubes made with `copy`, meshed, then their coincident nodes merged
(54 nodes before `merg n all`, 45 after):

```fbd
# expect: all.msh nodes=45 elements=16 C3D8=16 bbox=0,0,0,20,10,10
pnt p1 0 0 0
pnt p2 10 0 0
pnt p3 10 10 0
pnt p4 0 10 0
line l1 p1 p2 2
line l2 p2 p3 2
line l3 p3 p4 2
line l4 p4 p1 2
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 2
seta cube1 se all
comp cube1 do
copy cube1 cube2 tra 10 0 0
elty all he8
mesh all
merg n all
send all abq
```

## Export for CalculiX: send

`send all abq` writes the mesh (`*NODE, NSET=Nall` and `*ELEMENT, TYPE=..., ELSET=Eall`) to `all.msh`.
`send <set> abq nam` writes `<set>.nam` with `*NSET,NSET=N<set>` and/or `*ELSET,ELSET=E<set>`
for the nodes and elements in the set; `send all abq nam` writes one `.nam` file for every set.
`send <set> abq sur` writes `<set>.sur` with `*SURFACE, NAME=S<set>` from the element faces in
the set (only free faces count; use `comp <set> f` first).
`send <set> abq pres <value>` writes `<set>.dlo` with `*DLOAD` data lines `element, P<face>, value`.
`send <set> abq spc <dofs> [<value>]` writes `<set>_<dofs>.bou` with `*BOUNDARY` data lines
`node, dof, , [value]`. Neither file has the keyword line: write it in the deck before the include.
`send <set> abq force <fx> <fy> <fz>` writes `<set>.frc` with `*CLOAD` data lines
`node, dof, value` but no keyword line (the same force on every node of the set; zero components
are left out).
Use these files with `*INCLUDE, INPUT=<file>` in the deck (put `*CLOAD` before the `.frc` include).

Element set by number range and a nodal force on the end face of a cube:

```fbd
# expect: blk.nam EBLK=4
# expect: tip.nam NTIP=9
# expect: tip.frc exists
pnt p1 0 0 0
pnt p2 10 0 0
pnt p3 10 10 0
pnt p4 0 10 0
line l1 p1 p2 2
line l2 p2 p3 2
line l3 p3 p4 2
line l4 p4 p1 2
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 2
elty all he8
mesh all
seta nodes n all
enq nodes tip rec 10 _ _ 0.01 a
seta blk e 1 - 4 1
send blk abq nam
send tip abq nam
send tip abq force 0 0 -1
```

Block with a clamped end (x = 0) and a face set for a pressure load on the other end (x = 100):

```fbd
# expect: all.msh nodes=321 elements=40 C3D20=40
# expect: fix.nam NFIX=21
# expect: load.sur SLOAD=4
# expect: load.dlo exists
# expect: fix_123.bou exists
pnt p1 0 0 0
pnt p2 100 0 0
pnt p3 100 20 0
pnt p4 0 20 0
line l1 p1 p2 20
line l2 p2 p3 4
line l3 p3 p4 20
line l4 p4 p1 4
surf s1 blend l1 l2 l3 l4
seta base s s1
swep base top tra 0 0 10 4
elty all he20
mesh all
seta nodes n all
enq nodes fix rec 0 _ _ 0.01 a
enq nodes load rec 100 _ _ 0.01 a
comp load f
send all abq
send fix abq nam
send fix abq spc 123
send load abq sur
send load abq pres 1.5
```

A minimal deck around these files (write it next to them, then call `mesh_check` on it):

```
*INCLUDE, INPUT=all.msh
*INCLUDE, INPUT=fix.nam
*INCLUDE, INPUT=load.sur
*MATERIAL, NAME=STEEL
*ELASTIC
210000, 0.3
*DENSITY
7.85e-9
*SOLID SECTION, ELSET=Eall, MATERIAL=STEEL
*STEP
*STATIC
*BOUNDARY
Nfix, 1, 3
*DLOAD
Sload, P, 1.5
*NODE FILE
U
*EL FILE
S
*END STEP
```

## Values: valu

`valu <name> <value>` stores a string; afterwards every parameter equal to `<name>` is replaced by
it (see the parametric block above). `valu <name> * <a> <b>` (also `+ - /`, `sqr`, `sin`, `cos`,
...) computes; `valu <name> & <a> <b>` concatenates. Use names of several letters so that they do
not collide with command parameters such as `l` or `p`.
