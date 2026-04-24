// C wrapper around Xyce::Circuit::Simulator C++ methods.
// Provides the xyce_* C API declared in N_CIR_XyceCInterface.h.

#include <N_CIR_Xyce.h>

#include <cstdlib>
#include <cstring>
#include <map>
#include <string>
#include <utility>
#include <vector>
#include <unistd.h>

using Sim = Xyce::Circuit::Simulator;

extern "C" {

__attribute__((visibility("default")))
void xyce_open(void **ptr) {
    *ptr = static_cast<void *>(new Sim());
}

__attribute__((visibility("default")))
void xyce_close(void **ptr) {
    if (ptr && *ptr) {
        delete static_cast<Sim *>(*ptr);
        *ptr = nullptr;
    }
}

__attribute__((visibility("default")))
int xyce_initialize(void **ptr, int narg, char **argv) {
    return static_cast<Sim *>(*ptr)->initialize(narg, argv);
}

__attribute__((visibility("default")))
int xyce_runSimulation(void **ptr) {
    return static_cast<Sim *>(*ptr)->runSimulation();
}

__attribute__((visibility("default")))
int xyce_simulateUntil(void **ptr, double requestedUntilTime,
                       double *completedUntilTime) {
    return static_cast<Sim *>(*ptr)->simulateUntil(
        requestedUntilTime, *completedUntilTime);
}

__attribute__((visibility("default")))
double xyce_getTime(void **ptr) {
    return static_cast<Sim *>(*ptr)->getTime();
}

__attribute__((visibility("default")))
double xyce_getFinalTime(void **ptr) {
    return static_cast<Sim *>(*ptr)->getFinalTime();
}

__attribute__((visibility("default")))
int xyce_updateTimeVoltagePairs(void **ptr, char *DACname, int numPoints,
                                double *timeArray, double *voltageArray) {
    auto *sim = static_cast<Sim *>(*ptr);

    std::string name(DACname);
    auto *tvVec = new std::vector<std::pair<double, double>>();
    tvVec->reserve(numPoints);
    for (int i = 0; i < numPoints; i++) {
        tvVec->push_back({timeArray[i], voltageArray[i]});
    }

    std::map<std::string, std::vector<std::pair<double, double>> *> tvpMap;
    tvpMap[name] = tvVec;

    int result = sim->updateTimeVoltagePairs(tvpMap);

    delete tvVec;
    return result;
}

__attribute__((visibility("default")))
int xyce_checkResponseVar(void **ptr, char *variable_name) {
    return static_cast<Sim *>(*ptr)->checkResponseVar(
        std::string(variable_name));
}

__attribute__((visibility("default")))
int xyce_obtainResponse(void **ptr, char *variable_name, double *value) {
    return static_cast<Sim *>(*ptr)->obtainResponse(
        std::string(variable_name), *value);
}

__attribute__((visibility("default")))
double xyce_getCircuitValue(void **ptr, char *paramName) {
    double value = 0.0;
    static_cast<Sim *>(*ptr)->getCircuitValue(std::string(paramName), value);
    return value;
}

__attribute__((visibility("default")))
void xyce_set_working_directory(void ** /*ptr*/, const char *dirName) {
    if (dirName && dirName[0]) {
        (void)chdir(dirName);
    }
}

} // extern "C"
