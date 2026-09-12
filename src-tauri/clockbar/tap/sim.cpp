// sim.cpp — 离线回放 tap 日志的 TREE 事件流，验证 locate 扫描逻辑（诊断工具，不进 explorer）
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <string>
#include <unordered_map>
#include <vector>

struct Node { std::string type, name; unsigned long long parent; };
static std::unordered_map<unsigned long long, Node> g_nodes;
static bool g_located = false;
static unsigned long long g_hTime = 0;

static bool chain_matches(const std::unordered_map<unsigned long long, Node>& m,
                          unsigned long long hTime, unsigned long long hDate) {
    auto itT = m.find(hTime); if (itT == m.end()) { printf("      CM fail T-missing\n"); return false; }
    if (itT->second.name != "TimeInnerTextBlock") { printf("      CM fail T-name=[%s]\n", itT->second.name.c_str()); return false; }
    auto itS = m.find(itT->second.parent); if (itS == m.end()) { printf("      CM fail S-missing\n"); return false; }
    if (itS->second.type.find("StackPanel") == std::string::npos) { printf("      CM fail S-type=[%s]\n", itS->second.type.c_str()); return false; }
    if (!itS->second.name.empty()) { printf("      CM fail S-name=[%s]\n", itS->second.name.c_str()); return false; }
    auto itC = m.find(itS->second.parent); if (itC == m.end()) { printf("      CM fail C-missing\n"); return false; }
    if (itC->second.name != "ContainerGrid") { printf("      CM fail C-name=[%s]\n", itC->second.name.c_str()); return false; }
    auto itD = m.find(itC->second.parent); if (itD == m.end()) { printf("      CM fail D-missing\n"); return false; }
    if (itD->second.type != "SystemTray.DateTimeIconContent") { printf("      CM fail D-type=[%s]\n", itD->second.type.c_str()); return false; }
    auto itDt = m.find(hDate);
    if (itDt == m.end()) { printf("      CM fail Dt-missing\n"); return false; }
    if (itDt->second.name != "DateInnerTextBlock") { printf("      CM fail Dt-name=[%s]\n", itDt->second.name.c_str()); return false; }
    if (itDt->second.parent != itS->second.parent) { printf("      CM fail Dt-parent %llX != %llX\n", itDt->second.parent, itS->second.parent); return false; }
    return true;
}

struct Ev { bool add; unsigned long long h, parent; std::string type, name; };

int main(int argc, char** argv) {
    if (argc < 2) { printf("usage: sim <logfile>\n"); return 1; }
    FILE* f = fopen(argv[1], "r");
    if (!f) { printf("cannot open\n"); return 1; }
    std::vector<Ev> evs;
    char line[1024];
    while (fgets(line, sizeof(line), f)) {
        char* tp = strstr(line, "] TREE "); if (!tp) continue; tp += 7;
        bool add = (strncmp(tp, "ADD", 3) == 0);
        unsigned long long h = 0, parent = 0; unsigned idx = 0;
        char type[128] = "", name[128] = "";
        { char* nc = strstr(tp, " nchild="); if (nc) *nc = 0; }
        int n = sscanf(tp, "%*s h=%llX parent=%llX idx=%u type=%127s name=%127s",
                       &h, &parent, &idx, type, name);
        if (n < 4) continue;
        if (n == 4) name[0] = 0;
        evs.push_back({add, h, parent, type, name});
    }
    fclose(f);
    printf("parsed %zu events\n", evs.size());
    for (auto& e : evs) {
        if (e.add) {
            Node& nd = g_nodes[e.h];
            nd.type = e.type; nd.name = e.name; nd.parent = e.parent;
            if (!g_located) {
                for (auto& [hT, nT] : g_nodes) {
                    if (nT.name != "TimeInnerTextBlock") continue;
                    printf("SCAN candidate Time h=%llX parent=%llX\n", hT, nT.parent);
                    for (auto& [hD, nD] : g_nodes) {
                        if (nD.name != "DateInnerTextBlock") continue;
                        printf("SCAN   Date cand h=%llX parent=%llX\n", hD, nD.parent);
                        auto s1 = g_nodes.find(hT);
                        if (s1 == g_nodes.end()) { printf("SCAN   time missing\n"); break; }
                        auto s2 = g_nodes.find(s1->second.parent);
                        if (s2 == g_nodes.end()) { printf("SCAN   Stack MISSING\n"); break; }
                        printf("SCAN   Stack h=%llX type=[%s] name=[%s] isStack=%d nameEmpty=%d\n",
                               s1->second.parent, s2->second.type.c_str(), s2->second.name.c_str(),
                               (int)(s2->second.type.find("StackPanel") != std::string::npos),
                               (int)s2->second.name.empty());
                        auto s3 = g_nodes.find(s2->second.parent);
                        if (s3 == g_nodes.end()) { printf("SCAN   Cont MISSING\n"); break; }
                        printf("SCAN   Cont name=[%s]\n", s3->second.name.c_str());
                        auto s4 = g_nodes.find(s3->second.parent);
                        if (s4 == g_nodes.end()) { printf("SCAN   DTIC MISSING\n"); break; }
                        printf("SCAN   DTIC type=[%s] dateParentEq=%d match=%d\n",
                               s4->second.type.c_str(),
                               (int)(nD.parent == s1->second.parent),
                               (int)chain_matches(g_nodes, hT, hD));
                        break;
                    }
                    if (g_located) { g_hTime = hT; break; }
                }
                // 置位逻辑（与 tap.cpp 相同的判定）
                if (!g_located) {
                    for (auto& [hT, nT] : g_nodes) {
                        if (nT.name != "TimeInnerTextBlock") continue;
                        for (auto& [hD, nD] : g_nodes) {
                            if (nD.name == "DateInnerTextBlock" && chain_matches(g_nodes, hT, hD)) {
                                g_hTime = hT; g_located = true; break;
                            }
                        }
                        if (g_located) break;
                    }
                }
            }
        } else {
            g_nodes.erase(e.h);
        }
    }
    printf("located=%d nodes=%zu hTime=%llX\n", (int)g_located, g_nodes.size(), g_hTime);
    return 0;
}
