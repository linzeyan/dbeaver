// Baseline probe for the JDBC path DBeaver uses.
//
// Mirrors the Rust benchmark's two modes so the numbers are comparable:
//   stream  - read every cell, keep nothing
//   retain  - materialize each row as Object[] and hold it, which is what a
//             grid backed by row objects actually costs
//
// Usage: java -cp postgresql.jar JdbcBench.java [fetchSize] [--retain]

import java.sql.*;
import java.util.ArrayList;
import java.util.List;

public class JdbcBench {
    static final String URL =
        "jdbc:postgresql://127.0.0.1:55432/bench?user=bench&password=bench";
    static final String SQL = "SELECT * FROM bench_wide";

    public static void main(String[] args) throws Exception {
        int fetchSize = args.length > 0 && !args[0].startsWith("--")
            ? Integer.parseInt(args[0]) : 8192;
        boolean retain = false;
        for (String a : args) if (a.equals("--retain")) retain = true;

        long t0 = System.nanoTime();
        // autoCommit must be off or the PostgreSQL driver ignores fetchSize and
        // buffers the entire result client-side.
        Connection conn = DriverManager.getConnection(URL);
        conn.setAutoCommit(false);
        double connectMs = (System.nanoTime() - t0) / 1e6;

        long t1 = System.nanoTime();
        PreparedStatement ps = conn.prepareStatement(SQL);
        ps.setFetchSize(fetchSize);
        double prepareMs = (System.nanoTime() - t1) / 1e6;

        long t2 = System.nanoTime();
        ResultSet rs = ps.executeQuery();
        ResultSetMetaData md = rs.getMetaData();
        int cols = md.getColumnCount();

        double firstRowMs = Double.NaN;
        long rows = 0;
        List<Object[]> held = retain ? new ArrayList<>(1_000_000) : null;

        while (rs.next()) {
            if (rows == 0) firstRowMs = (System.nanoTime() - t2) / 1e6;
            Object[] row = new Object[cols];
            for (int i = 1; i <= cols; i++) row[i - 1] = rs.getObject(i);
            if (retain) held.add(row);
            rows++;
        }
        double totalS = (System.nanoTime() - t2) / 1e9;

        Runtime rt = Runtime.getRuntime();
        long usedMb = (rt.totalMemory() - rt.freeMemory()) / (1024 * 1024);

        System.out.printf("columns          %d%n", cols);
        System.out.printf("fetch_size       %d%n", fetchSize);
        System.out.printf("connect_ms       %.1f%n", connectMs);
        System.out.printf("prepare_ms       %.1f%n", prepareMs);
        System.out.printf("first_row_ms     %.1f%n", firstRowMs);
        System.out.printf("rows             %d%n", rows);
        System.out.printf("retained         %d%n", retain ? held.size() : 0);
        System.out.printf("heap_used_mb     %d%n", usedMb);
        System.out.printf("total_s          %.3f%n", totalS);
        System.out.printf("rows_per_s       %.0f%n", rows / totalS);

        rs.close(); ps.close(); conn.close();
    }
}
