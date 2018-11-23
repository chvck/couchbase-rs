// This file was generated by queries.rb
typedef struct analytics_query_str { const char *comment; size_t query_len; const char *query; } analytics_query_str;
size_t num_queries = 27;
analytics_query_str queries[28] = {
{"Display dataverses defined by Analytics service",
 50,
 "{\"statement\":\"SELECT * FROM Metadata.`Dataverse`\"}"},

{"Tell analytics service to shadow the data from beer-sample bucket using breweries dataset.",
 84,
 "{\"statement\":\"CREATE DATASET breweries ON `beer-sample` WHERE `type` = \\\"brewery\\\"\"}"},

{"Tell analytics service to shadow the data from beer-sample bucket using beers dataset.",
 77,
 "{\"statement\":\"CREATE DATASET beers ON `beer-sample` WHERE `type` = \\\"beer\\\"\"}"},

{"Initiate the shadowing relationship of datasets to the data in Couchbase Server",
 34,
 "{\"statement\":\"CONNECT LINK Local\"}"},

{"List populated datasets",
 129,
 "{\"statement\":\"SELECT ds.BucketName, ds.DatasetName, ds.`Filter` FROM Metadata.`Dataset` ds WHERE ds.DataverseName = \\\"Default\\\"\"}"},

{"Ask Analytics the number of breweries",
 52,
 "{\"statement\":\"SELECT VALUE COUNT(*) FROM breweries\"}"},

{"Retrieve first brewery, ordered by name",
 75,
 "{\"statement\":\"SELECT * FROM breweries ORDER BY name LIMIT 1\",\"pretty\":true}"},

{"Find a particular brewery based on its Couchbase Server key",
 118,
 "{\"statement\":\"SELECT meta(bw) AS meta, bw AS data FROM breweries bw WHERE meta(bw).id = 'kona_brewing'\",\"pretty\":true}"},

{"Find the same brewery information but in a slightly simpler or cleaner way based only on the data",
 94,
 "{\"statement\":\"SELECT VALUE bw FROM breweries bw WHERE bw.name = 'Kona Brewing'\",\"pretty\":true}"},

{"Find brewery by name, but return objects instead of values",
 88,
 "{\"statement\":\"SELECT bw FROM breweries bw WHERE bw.name = 'Kona Brewing'\",\"pretty\":true}"},

{"Apply a range condition together with a string condition to select breweries",
 133,
 "{\"statement\":\"SELECT VALUE bw FROM breweries bw WHERE bw.geo.lat > 60.0 AND bw.name LIKE '%Brewing%' ORDER BY bw.name\",\"pretty\":true}"},

{"Fetch  list of all breweries paired with their associated beers, with the list enumerating the brewery name and the beer name for each such pair, while also limiting the answer set size to at most 3 results.",
 153,
 "{\"statement\":\"SELECT bw.name AS brewer, br.name AS beer FROM breweries bw, beers br WHERE br.brewery_id = meta(bw).id ORDER BY bw.name, br.name LIMIT 3\"}"},

{"Fetch  list of all breweries paired with their associated beers, including all attributes, while also limiting the answer set size to at most 3 results.",
 134,
 "{\"statement\":\"SELECT * FROM breweries bw, beers br WHERE br.brewery_id = meta(bw).id ORDER BY bw.name, br.name LIMIT 3\",\"pretty\":true}"},

{"Fetch  list of all breweries paired with their associated beers, including all attributes, while also limiting the answer set size to at most 3 results. With ANSI JOINS!",
 135,
 "{\"statement\":\"SELECT * FROM breweries bw JOIN beers br ON br.brewery_id = meta(bw).id ORDER BY bw.name, br.name LIMIT 3\",\"pretty\":true}"},

{"Fetch  list of all breweries paired with their associated beers, including all attributes, while also limiting the answer set size to at most 3 results. Select values explicitly.",
 163,
 "{\"statement\":\"SELECT VALUE {\\\"bw\\\": bw, \\\"br\\\": br} FROM breweries bw, beers br WHERE br.brewery_id = meta(bw).id ORDER BY bw.name, br.name LIMIT 3\",\"pretty\":true}"},

{"For each brewery produce an object that contains the brewery name along with a list of all of the brewery’s offered beer names and alcohol percentages",
 197,
 "{\"statement\":\"SELECT bw.name AS brewer, (SELECT br.name, br.abv FROM beers br WHERE br.brewery_id = meta(bw).id ORDER BY br.name) AS beers FROM breweries bw ORDER BY bw.name LIMIT 2\",\"pretty\":true}"},

{"For each Arizona brewery get the brewery's name, location, and a list of competitors' names -- where competitors are other breweries that are geographically close to their location",
 319,
 "{\"statement\":\"SELECT bw1.name AS brewer, bw1.geo AS location, (SELECT VALUE bw2.name FROM breweries bw2 WHERE bw2.name != bw1.name AND abs(bw1.geo.lat - bw2.geo.lat) <= 0.1 AND abs(bw2.geo.lon - bw1.geo.lon) <= 0.1) AS competitors FROM breweries bw1 WHERE bw1.state = 'Arizona' ORDER BY bw1.name LIMIT 3\",\"pretty\":true}"},

{"Find those breweries whose beers include at least one IPA and return the brewery’s name, phone number, and complete list of beer names and associated alcohol levels.",
 336,
 "{\"statement\":\"WITH nested_breweries AS ( SELECT bw.name AS brewer, bw.phone, ( SELECT br.name, br.abv FROM beers br WHERE br.brewery_id = meta(bw).id ORDER BY br.name) AS beers FROM breweries bw) SELECT VALUE nb FROM nested_breweries nb WHERE (SOME b IN nb.beers SATISFIES b.name LIKE '%IPA%') ORDER BY nb.brewer LIMIT 2\",\"pretty\":true}"},

{"Find those breweries that only have seriously strong beers",
 359,
 "{\"statement\":\"WITH nested_breweries AS ( SELECT bw.name AS brewer, bw.phone, ( SELECT br.name, br.abv FROM beers br WHERE br.brewery_id = meta(bw).id ORDER BY br.name) AS beers FROM breweries bw) SELECT VALUE nb FROM nested_breweries nb WHERE (EVERY b IN nb.beers SATISFIES b.abv >= 10) AND ARRAY_COUNT(nb.beers) > 0 ORDER BY nb.brewer LIMIT 5\",\"pretty\":true}"},

{"Compute the total number of beers in a SQL-like way",
 55,
 "{\"statement\":\"SELECT COUNT(*) AS num_beers FROM beers\"}"},

{"Compute the total number of beers and return unwrapped value",
 50,
 "{\"statement\":\"SELECT VALUE COUNT(b) FROM beers b\"}"},

{"Compute the total number of beers with explicit aggregate function",
 65,
 "{\"statement\":\"SELECT VALUE ARRAY_COUNT((SELECT b FROM beers b))\"}"},

{"For each brewery that offers more than 30 beers, the following group-by or aggregate query reports the number of beers that it offers",
 140,
 "{\"statement\":\"SELECT br.brewery_id, COUNT(*) AS num_beers FROM beers br GROUP BY br.brewery_id HAVING COUNT(*) > 30 ORDER BY COUNT(*) DESC\"}"},

{"For each brewery that offers more than 30 beers, the following group-by or aggregate query reports the number of beers that it offers. With a hash-based aggregation hint.",
 152,
 "{\"statement\":\"SELECT br.brewery_id, COUNT(*) AS num_beers FROM beers br /*+ hash */ GROUP BY br.brewery_id HAVING COUNT(*) > 30 ORDER BY COUNT(*) DESC\"}"},

{"Return the top three breweries based on their numbers of offered beers. Also illustrate the use of multiple aggregate functions to compute various alcohol content statistics for their beers",
 236,
 "{\"statement\":\"SELECT bw.name, COUNT(*) AS num_beers, AVG(br.abv) AS abv_avg, MIN(br.abv) AS abv_min, MAX(br.abv) AS abv_max FROM breweries bw, beers br WHERE br.brewery_id = meta(bw).id GROUP BY bw.name ORDER BY num_beers DESC LIMIT 3\"}"},

{"Find the same brewery information but specify name as parameter",
 108,
 "{\"statement\":\"SELECT VALUE bw FROM breweries bw WHERE bw.name = $name\",\"$name\":\"Kona Brewing\",\"pretty\":true}"},

{"Find the same brewery information but specify name as parameter",
 105,
 "{\"statement\":\"SELECT VALUE bw FROM breweries bw WHERE bw.name = ?\",\"args\":[\"Kona Brewing\"],\"pretty\":true}"},

{NULL, 0, NULL}};