-- The pg_automerge extension's catalog, one line per fact, sorted: what
-- tests/upgrade.sh and tests/docker_upgrade.sh compare between an updated
-- and a freshly created extension.
--
-- Members are the objects recorded with deptype 'e'. Their dependencies
-- are every other pg_depend row of a member, plus the internal ones of
-- objects that belong to a member (the type's array type), so a user's
-- tables, indexes, views and triggers on the type, which an updated
-- database has and a fresh one has not, stay out. Then function
-- definitions (labels, language, symbol) with their ACLs, aggregates,
-- types, casts, operators, and the extension row.
WITH ext AS (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge'),
members AS (
    SELECT d.classid, d.objid FROM pg_depend d, ext
    WHERE d.refclassid = 'pg_extension'::regclass AND d.refobjid = ext.oid AND d.deptype = 'e'
)
SELECT 'member ' || pg_describe_object(classid, objid, 0)
       || coalesce(' -- ' || obj_description(objid, (SELECT relname FROM pg_class WHERE oid = classid)::name), '')
FROM members
UNION ALL
SELECT format('depends %s -> %s (%s)', pg_describe_object(d.classid, d.objid, d.objsubid),
              pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid), d.deptype)
FROM pg_depend d
WHERE d.deptype <> 'e'
  AND ((d.classid, d.objid) IN (SELECT classid, objid FROM members)
       OR (d.deptype = 'i' AND (d.refclassid, d.refobjid) IN (SELECT classid, objid FROM members)))
UNION ALL
SELECT 'function ' || p.oid::regprocedure || ' acl=' || coalesce(p.proacl::text, 'default') || ' ' ||
       CASE WHEN p.prokind = 'a' THEN 'aggregate' ELSE pg_get_functiondef(p.oid) END
FROM members m JOIN pg_proc p ON m.classid = 'pg_proc'::regclass AND p.oid = m.objid
UNION ALL
SELECT format('aggregate %s kind=%s trans=%s final=%s finalextra=%s finalmodify=%s combine=%s serial=%s deserial=%s '
              'mtrans=%s minv=%s mfinal=%s sort=%s stype=%s space=%s init=%s',
              a.aggfnoid::regprocedure, a.aggkind, a.aggtransfn, a.aggfinalfn, a.aggfinalextra,
              a.aggfinalmodify, a.aggcombinefn, a.aggserialfn, a.aggdeserialfn, a.aggmtransfn,
              a.aggminvtransfn, a.aggmfinalfn, a.aggsortop, a.aggtranstype::regtype, a.aggtransspace,
              a.agginitval)
FROM members m JOIN pg_aggregate a ON m.classid = 'pg_proc'::regclass AND a.aggfnoid = m.objid
UNION ALL
SELECT format('type %s len=%s byval=%s align=%s storage=%s in=%s out=%s recv=%s send=%s modin=%s modout=%s '
              'analyze=%s subscript=%s cat=%s preferred=%s delim=%s elem=%s array=%s acl=%s',
              t.oid::regtype, t.typlen, t.typbyval, t.typalign, t.typstorage, t.typinput, t.typoutput,
              t.typreceive, t.typsend, t.typmodin, t.typmodout, t.typanalyze, t.typsubscript,
              t.typcategory, t.typispreferred, t.typdelim, t.typelem::regtype, t.typarray::regtype,
              coalesce(t.typacl::text, 'default'))
FROM members m JOIN pg_type t ON m.classid = 'pg_type'::regclass AND t.oid = m.objid
UNION ALL
SELECT format('cast %s -> %s func=%s context=%s method=%s',
              c.castsource::regtype, c.casttarget::regtype, c.castfunc::regprocedure,
              c.castcontext, c.castmethod)
FROM members m JOIN pg_cast c ON m.classid = 'pg_cast'::regclass AND c.oid = m.objid
UNION ALL
SELECT format('operator %s(%s, %s) kind=%s result=%s code=%s com=%s neg=%s rest=%s join=%s merges=%s hashes=%s',
              o.oprname, o.oprleft::regtype, o.oprright::regtype, o.oprkind, o.oprresult::regtype,
              o.oprcode, o.oprcom::regoperator, o.oprnegate::regoperator, o.oprrest, o.oprjoin,
              o.oprcanmerge, o.oprcanhash)
FROM members m JOIN pg_operator o ON m.classid = 'pg_operator'::regclass AND o.oid = m.objid
UNION ALL
SELECT 'extension ' || extname || ' ' || extversion || ' relocatable=' || extrelocatable
       || ' schema=' || extnamespace::regnamespace || ' config=' || coalesce(extconfig::text, '')
       || ' condition=' || coalesce(extcondition::text, '')
FROM pg_extension WHERE extname = 'pg_automerge'
ORDER BY 1;
