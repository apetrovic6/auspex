SELECT machine,
       count(*)                                                       AS actions,
       coalesce(sum(kind = 'deploy' AND result = 'succeeded'), 0)     AS deploys_ok,
       coalesce(sum(result IN ('failed', 'timed_out')), 0)            AS failures,
       coalesce(sum(result = 'interrupted'
                    OR (result IS NULL
                        AND started_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-24 hours'))), 0)
                                                                      AS interrupted
FROM actions
WHERE started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-7 days')
GROUP BY machine
ORDER BY machine;
