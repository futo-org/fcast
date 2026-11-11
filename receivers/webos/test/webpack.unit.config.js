const path = require('path');

// Run from the service folder: `npm run build:unit` in receivers/webos/fcast-receiver-service.
const serviceDir = path.resolve(__dirname, '../fcast-receiver-service');
const commonWebDir = path.resolve(__dirname, '../../common/web');
const serviceModules = path.resolve(serviceDir, 'node_modules');

// webpack is installed in the service folder, not next to this config.
const webpack = require(require.resolve('webpack', { paths: [serviceDir] }));

module.exports = {
    context: __dirname,
    mode: 'development',
    devtool: false,
    target: 'node12',
    entry: './unit/index.ts',
    module: {
        rules: [
            {
                test: /\.tsx?$/,
                include: [__dirname, commonWebDir, path.resolve(serviceDir, 'src')],
                use: [
                    {
                        loader: 'ts-loader',
                        options: {
                            transpileOnly: true,
                            configFile: path.resolve(serviceDir, 'tsconfig.json'),
                        },
                    },
                ],
            },
        ],
    },
    resolve: {
        alias: {
            'src/Main$': path.resolve(__dirname, 'unit/main-stub.ts'),
            'src': path.resolve(serviceDir, 'src'),
            'modules': serviceModules,
            'common': commonWebDir,
        },
        extensions: ['.tsx', '.ts', '.js'],
        extensionAlias: { '.js': ['.ts', '.js'] },
        modules: [serviceModules, 'node_modules'],
    },
    resolveLoader: {
        modules: [serviceModules, 'node_modules'],
    },
    output: {
        filename: 'unit-tests.js',
        path: path.resolve(__dirname, 'dist'),
    },
    plugins: [
        new webpack.DefinePlugin({
            TARGET: JSON.stringify('webOS'),
        }),
    ],
};
